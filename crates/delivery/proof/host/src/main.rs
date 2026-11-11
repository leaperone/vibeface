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

struct DiagnosticStage {
    file: Option<std::fs::File>,
    phase: &'static str,
    command: &'static str,
    success: bool,
    errno: Option<i32>,
    order: bool,
}
fn diagnostic_file(fd: i32) -> Option<std::fs::File> {
    use std::os::{fd::BorrowedFd, unix::fs::MetadataExt};
    if fd < 3 {
        return None;
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0
        || flags & libc::O_APPEND == 0
        || flags & libc::O_ACCMODE == libc::O_RDONLY
        || unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC != 0
    {
        return None;
    }
    let file = std::fs::File::from(
        unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .ok()?,
    );
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > 32768
        || metadata.nlink() != 1
    {
        return None;
    }
    Some(file)
}

impl DiagnosticStage {
    fn new(phase: &'static str, command: &str) -> Self {
        let format = diagnostic_format(
            std::env::var("VIBEFACE_DELIVERY_DIAGNOSTIC_FORMAT")
                .ok()
                .as_deref(),
        );
        let file = format.and_then(|_| {
            std::env::var("VIBEFACE_DELIVERY_DIAGNOSTIC_FD")
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
                .and_then(diagnostic_file)
        });
        let stage = Self {
            file,
            phase,
            command: if command == "verify" {
                "verify"
            } else {
                "prove"
            },
            success: false,
            errno: None,
            order: format == Some(true),
        };
        stage.emit("begin");
        stage
    }
    fn counters(&self) -> Option<serde_json::Value> {
        if !matches!(self.phase, "host_succinct" | "host_groth16") {
            return None;
        }
        let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?;
        let root = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        Self::counters_at(&root)
    }
    fn counters_at(root: &Path) -> Option<serde_json::Value> {
        let read = |name: &str| -> Option<std::collections::HashMap<String, u64>> {
            let mut values = std::collections::HashMap::new();
            for line in std::fs::read_to_string(root.join(name)).ok()?.lines() {
                let (key, value) = line.split_once(' ')?;
                let value = value.parse::<u64>().ok()?;
                if value > i64::MAX as u64 || values.insert(key.to_owned(), value).is_some() {
                    return None;
                }
            }
            Some(values)
        };
        let memory = read("memory.events")?;
        let pids = read("pids.events")?;
        Some(
            json!({"pids_max":pids.get("max")?,"memory_max":memory.get("max")?,"oom":memory.get("oom")?,
            "oom_kill":memory.get("oom_kill")?,"oom_group_kill":memory.get("oom_group_kill")?}),
        )
    }
    fn emit(&self, edge: &str) {
        let Some(file) = &self.file else {
            return;
        };
        let value = if self.order {
            use std::os::unix::fs::MetadataExt;
            let group = std::fs::read_to_string("/proc/self/cgroup")
                .ok()
                .and_then(|text| {
                    text.lines().find_map(|line| {
                        line.strip_prefix("0::").map(|relative| {
                            Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'))
                        })
                    })
                });
            let identity = group
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|info| [info.dev(), info.ino()]);
            let mut clock = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let time = (unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } == 0)
                .then(|| {
                    u64::try_from(clock.tv_sec)
                        .ok()?
                        .checked_mul(1_000_000_000)?
                        .checked_add(u64::try_from(clock.tv_nsec).ok()?)
                })
                .flatten()
                .filter(|value| *value <= i64::MAX as u64);
            let counters = group
                .as_deref()
                .filter(|_| matches!(self.phase, "host_succinct" | "host_groth16"))
                .and_then(Self::counters_at)
                .and_then(|value| {
                    Some([
                        value["pids_max"].as_u64()?,
                        value["memory_max"].as_u64()?,
                        value["oom"].as_u64()?,
                        value["oom_kill"].as_u64()?,
                        value["oom_group_kill"].as_u64()?,
                    ])
                });
            json!({"v":2,"kind":"event","w":"host","c":self.command,"p":self.phase,"e":edge,
                "t":time,"x":null,"s":null,"n":self.errno,"f":if edge == "error" {Some("stage_error")} else {None},
                "r":null,"k":false,"g":identity,"q":counters,"i":null,"a":null})
        } else {
            json!({"v":1,"writer":"host","command":self.command,"phase":self.phase,"edge":edge,
            "exit_code":null,"signal":null,"errno":self.errno,"failure_class":if edge == "error" {Some("stage_error")} else {None},
            "worker":null,"cleanup_requested":false,"counters":self.counters()})
        };
        let Ok(mut bytes) = serde_json::to_vec(&value) else {
            return;
        };
        bytes.push(b'\n');
        let Ok(metadata) = file.metadata() else {
            return;
        };
        if bytes.len() > 512 || metadata.len().saturating_add(bytes.len() as u64) > 32768 {
            let _ = file.set_len(32769);
            return;
        }
        let mut destination = file;
        if destination.write(&bytes).ok() != Some(bytes.len()) {
            let _ = file.set_len(32769);
        }
    }
    fn ok(&mut self) {
        self.success = true;
    }
    fn failure(&mut self, error: &anyhow::Error) {
        self.errno = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error);
    }
}
fn diagnostic_format(value: Option<&str>) -> Option<bool> {
    match value {
        None => Some(false),
        Some("order-job-v2") => Some(true),
        _ => None,
    }
}
impl Drop for DiagnosticStage {
    fn drop(&mut self) {
        self.emit(if self.success { "ok" } else { "error" });
    }
}

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
        let mut diagnostic = DiagnosticStage::new("host_receipt_verify", "verify");
        ensure!(
            args.len() == 2,
            "verify requires the expected public journal hex"
        );
        let statement = expected_statement(&args[1])?;
        let receipt = decode_receipt(&read_stdin(MAX_RECEIPT_INPUT)?)?;
        checked_receipt(&receipt, statement.abi_bytes())?;
        let seal = groth16_seal(&receipt)?;
        diagnostic.ok();
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
    let mut input_diagnostic = DiagnosticStage::new("host_input", command);
    let input: ProofInput = serde_json::from_slice(&read_stdin(MAX_PRIVATE_INPUT)?)
        .map_err(|_| anyhow::anyhow!("invalid private proof input"))?;
    input.profile.validate()?;
    ensure!(
        input.profile.guest_image_id.as_slice() == image_id(),
        "profile must bind this actual guest image"
    );
    let mut builder = ExecutorEnv::builder();
    builder
        .segment_limit_po2(19)
        .session_limit(Some(input.profile.max_guest_cycles))
        .write(&input)?;
    let env = builder.build()?;
    let prover = local_prover()?;
    input_diagnostic.ok();
    drop(input_diagnostic);
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
        let mut succinct_diagnostic = DiagnosticStage::new("host_succinct", command);
        let info = prover
            .prove_with_opts(env, VIBEFACE_DELIVERY_GUEST_ELF, &ProverOpts::succinct())
            .inspect_err(|error| {
                succinct_diagnostic.failure(error);
            })?;
        succinct_diagnostic.ok();
        drop(succinct_diagnostic);
        let mut succinct_verify = DiagnosticStage::new("host_succinct_verify", command);
        checked_succinct_receipt(&info.receipt, input.statement.abi_bytes())?;
        succinct_verify.ok();
        drop(succinct_verify);
        if let Some(path) = succinct {
            let bytes = bincode::serialize(&info.receipt)?;
            ensure!(
                bytes.len() as u64 <= MAX_RECEIPT_INPUT,
                "succinct receipt exceeds local limit"
            );
            publish_receipt(&path, &bytes)?;
        }
        let mut groth16_diagnostic = DiagnosticStage::new("host_groth16", command);
        let receipt = prover
            .compress(&ProverOpts::groth16(), &info.receipt)
            .inspect_err(|error| {
                groth16_diagnostic.failure(error);
            })?;
        groth16_diagnostic.ok();
        drop(groth16_diagnostic);
        let mut receipt_diagnostic = DiagnosticStage::new("host_receipt_verify", command);
        checked_receipt(&receipt, input.statement.abi_bytes())?;
        let seal = groth16_seal(&receipt)?;
        receipt_diagnostic.ok();
        drop(receipt_diagnostic);
        let mut publication = DiagnosticStage::new("host_receipt_publish", command);
        publish_receipt(&output, &bincode::serialize(&receipt)?)?;
        publication.ok();
        drop(publication);
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
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../../.artifacts/proof-host-tests")
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

    #[test]
    fn diagnostic_descriptor_is_private_append_only_and_optional() {
        use std::os::fd::AsRawFd;
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../.artifacts/proof-host-tests");
        std::fs::create_dir_all(&base).unwrap();
        let directory = base.join(format!(
            "diagnostic-{}-{}",
            std::process::id(),
            RECEIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("events.jsonl");
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let fd = file.as_raw_fd();
        let original = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(diagnostic_file(fd).is_none());
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, original & !libc::FD_CLOEXEC) },
            0
        );
        let mut stage = DiagnosticStage {
            file: diagnostic_file(fd),
            phase: "host_succinct",
            command: "prove",
            success: false,
            errno: None,
            order: false,
        };
        assert!(stage.file.is_some());
        stage.emit("begin");
        stage.ok();
        drop(stage);
        let events: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["edge"], "begin");
        assert_eq!(events[1]["edge"], "ok");
        assert!(
            events
                .iter()
                .all(|event| event["v"] == 1 && event.get("kind").is_none())
        );
        let linked = directory.join("linked");
        std::fs::hard_link(&path, &linked).unwrap();
        assert!(diagnostic_file(fd).is_none());
        std::fs::remove_file(linked).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(diagnostic_file(fd).is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        file.set_len(32769).unwrap();
        assert!(diagnostic_file(fd).is_none());
        drop(file);
        assert!(diagnostic_file(fd).is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn diagnostic_format_never_guesses_an_unknown_encoding() {
        assert_eq!(diagnostic_format(None), Some(false));
        assert_eq!(diagnostic_format(Some("order-job-v2")), Some(true));
        assert_eq!(diagnostic_format(Some("")), None);
        assert_eq!(diagnostic_format(Some("unknown")), None);
    }
}
