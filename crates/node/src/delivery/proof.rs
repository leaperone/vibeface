use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vibeface_delivery::{ProofInput, ProofProfile, verify_relation};
use zeroize::Zeroizing;

pub const INFO_TIMEOUT_SECS: u64 = 30;
pub const PROVE_TIMEOUT_SECS: u64 = 7200;
pub const VERIFY_TIMEOUT_SECS: u64 = 120;

#[derive(Debug)]
pub struct Busy;
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "a previously authorized proof is still running; waiting to reconcile its receipt",
        )
    }
}
impl std::error::Error for Busy {}

fn process_lock(receipt: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let directory = receipt
        .parent()
        .context("proof receipt has no private directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0,
        "proof receipt directory must be private"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("prover.lock"))?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.permissions().mode() & 0o077 == 0,
        "proof lock must be a private regular file"
    );
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(
        duplicate >= 0,
        "proof lock descriptor could not be reserved"
    );
    let file = unsafe { File::from_raw_fd(duplicate) };
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Busy.into()),
        Err(error) => Err(error.into()),
    }
}

fn inherit_lock(command: &mut tokio::process::Command, lock: &File) {
    let fd = lock.as_raw_fd();
    command.env("VIBEFACE_DELIVERY_PROOF_LOCK_FD", fd.to_string());
    // The trusted host and its local descendants retain ownership if this Node is killed.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofConfig {
    pub program: PathBuf,
    pub r0vm: PathBuf,
    pub cgroup: PathBuf,
    pub profile: ProofProfile,
}
impl ProofConfig {
    pub fn validate(&self) -> Result<()> {
        self.profile.validate()?;
        ensure!(
            self.program.is_absolute() && self.r0vm.is_absolute() && self.cgroup.is_absolute(),
            "proof paths must be absolute local paths"
        );
        Ok(())
    }
    fn validate_executables(&self) -> Result<()> {
        self.profile.validate()?;
        for path in [&self.program, &self.r0vm] {
            ensure!(
                path.is_absolute(),
                "proof program must be an absolute local path"
            );
            let metadata = std::fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "proof program must be a regular local executable"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o022 == 0,
                    "proof program is writable by other users"
                );
            }
        }
        Ok(())
    }
    pub fn readiness(&self) -> Result<()> {
        self.validate()?;
        self.validate_executables()?;
        self.check_memory_limit()
    }
    pub fn check_memory_limit(&self) -> Result<()> {
        ensure!(
            cfg!(target_os = "linux"),
            "paid proving requires a Linux cgroup memory limit"
        );
        ensure!(
            self.cgroup.is_absolute() && self.cgroup.starts_with("/sys/fs/cgroup"),
            "proof cgroup must be under the Linux cgroup v2 mount"
        );
        let metadata = std::fs::symlink_metadata(&self.cgroup)?;
        ensure!(
            self.cgroup
                .canonicalize()?
                .starts_with(Path::new("/sys/fs/cgroup").canonicalize()?),
            "proof cgroup escaped the kernel cgroup mount"
        );
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "proof cgroup must be a real directory"
        );
        let memory: u64 = std::fs::read_to_string(self.cgroup.join("memory.max"))?
            .trim()
            .parse()
            .context("proof cgroup has no hard memory limit")?;
        ensure!(
            memory > 0 && memory <= self.profile.max_prover_memory_bytes,
            "proof cgroup exceeds profile memory cap"
        );
        ensure!(
            std::fs::read_to_string(self.cgroup.join("memory.swap.max"))?.trim() == "0",
            "proof cgroup must disable swap"
        );
        let pids: u32 = std::fs::read_to_string(self.cgroup.join("pids.max"))?
            .trim()
            .parse()?;
        ensure!(
            pids > 0 && pids <= 64,
            "proof cgroup requires a bounded process count"
        );
        if !self.is_current_cgroup()? {
            ensure!(
                std::fs::read_to_string(self.cgroup.join("memory.oom.group"))?.trim() == "1",
                "dedicated proof cgroup must terminate the whole task on OOM"
            );
            ensure!(
                std::fs::read_to_string(self.cgroup.join("cgroup.procs"))?
                    .trim()
                    .is_empty(),
                "proof cgroup is already occupied"
            );
        }
        Ok(())
    }
    fn oom_events(&self) -> Result<u64> {
        let events = std::fs::read_to_string(self.cgroup.join("memory.events"))?;
        let mut total = 0u64;
        for line in events.lines() {
            if let Some((name, count)) = line.split_once(' ')
                && matches!(name, "oom" | "oom_kill" | "oom_group_kill")
            {
                total = total
                    .checked_add(count.parse()?)
                    .context("invalid cgroup OOM count")?;
            }
        }
        Ok(total)
    }
    fn is_current_cgroup(&self) -> Result<bool> {
        if !cfg!(target_os = "linux") {
            return Ok(false);
        }
        let membership = std::fs::read_to_string("/proc/self/cgroup")?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .context("process has no unified cgroup membership")?;
        let current = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        Ok(current.canonicalize()? == self.cgroup.canonicalize()?)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProverInfo {
    image_id: String,
    schema_hash: String,
}
#[derive(Deserialize)]
struct ProverOutput {
    mode: String,
    kind: String,
    #[serde(rename = "imageId")]
    image_id: String,
    #[serde(rename = "profileId")]
    profile_id: String,
    relation: u8,
    journal: String,
    seal: String,
}

pub struct ProofRunner {
    slot: tokio::sync::Semaphore,
}
impl Default for ProofRunner {
    fn default() -> Self {
        Self {
            slot: tokio::sync::Semaphore::new(1),
        }
    }
}
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        // The isolated group includes r0vm and the confined Groth16 worker.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
fn command(config: &ProofConfig, lock: &File) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(&config.program);
    command
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("RISC0_SERVER_PATH", &config.r0vm)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    if !config.is_current_cgroup()? {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let cgroup = CString::new(config.cgroup.join("cgroup.procs").as_os_str().as_bytes())?;
        unsafe {
            command.pre_exec(move || {
                let fd = libc::open(
                    cgroup.as_ptr(),
                    libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                );
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut digits = [0u8; 32];
                let mut cursor = digits.len();
                let mut pid = libc::getpid() as u32;
                loop {
                    cursor -= 1;
                    digits[cursor] = b'0' + (pid % 10) as u8;
                    pid /= 10;
                    if pid == 0 {
                        break;
                    }
                }
                let written =
                    libc::write(fd, digits[cursor..].as_ptr().cast(), digits.len() - cursor);
                libc::close(fd);
                if written != (digits.len() - cursor) as isize {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    inherit_lock(&mut command, lock);
    Ok(command)
}
async fn bounded_run(
    mut command: tokio::process::Command,
    input: &[u8],
    limit: usize,
    timeout: std::time::Duration,
) -> Result<Vec<u8>> {
    let mut child = command.spawn()?;
    let _group = ProcessGroup(child.id().context("proof task has no process identity")?);
    let mut stdin = child.stdin.take().context("proof stdin unavailable")?;
    let mut stdout = child.stdout.take().context("proof stdout unavailable")?;
    let run = async {
        let writer = async {
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        };
        let reader = async {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(bytes.len() <= limit, "proof task output exceeds its bound");
            Ok::<_, anyhow::Error>(bytes)
        };
        let (_, output) = tokio::try_join!(writer, reader)?;
        ensure!(
            child.wait().await?.success(),
            "proof task failed; order remains unresolved"
        );
        Ok::<_, anyhow::Error>(output)
    };
    tokio::time::timeout(timeout, run)
        .await
        .context("proof task exceeded its execution deadline")?
}
impl ProofRunner {
    pub async fn prove(
        &self,
        config: &ProofConfig,
        input: &ProofInput,
        receipt: &Path,
    ) -> Result<Vec<u8>> {
        let _slot = self
            .slot
            .try_acquire()
            .context("proof queue is busy; no charge was submitted")?;
        let lock = process_lock(receipt)?;
        config.validate()?;
        config.validate_executables()?;
        config.check_memory_limit()?;
        let oom_before = config.oom_events()?;
        ensure!(
            !Path::new("/var/run/docker.sock").exists(),
            "proof execution requires a confined Groth16 worker without the host Docker socket"
        );
        verify_relation(&input.profile, &input.statement, &input.witness)?;
        ensure!(
            input.profile == config.profile,
            "proof request mismatches local profile"
        );
        let mut identify = command(config, &lock)?;
        identify.arg("info");
        let info = bounded_run(
            identify,
            &[],
            4096,
            std::time::Duration::from_secs(INFO_TIMEOUT_SECS),
        )
        .await?;
        let info: ProverInfo =
            serde_json::from_slice(&info).context("invalid proof host identification")?;
        ensure!(
            info.image_id == hex::encode(input.profile.guest_image_id)
                && info.schema_hash == hex::encode(input.profile.statement_schema_hash),
            "proof host image or schema mismatch"
        );
        if receipt.try_exists()? {
            let seal = verified_seal(config, input, receipt, &lock).await?;
            ensure!(
                config.oom_events()? == oom_before,
                "proof memory limit was exceeded; receipt was not accepted"
            );
            return Ok(seal);
        }
        let mut prove = command(config, &lock)?;
        prove.arg("prove").arg(receipt);
        let private = Zeroizing::new(serde_json::to_vec(input)?);
        ensure!(
            private.len() <= 1024 * 1024,
            "proof input exceeds local bound"
        );
        let output = bounded_run(
            prove,
            &private,
            16_384,
            std::time::Duration::from_secs(PROVE_TIMEOUT_SECS),
        )
        .await?;
        let output: ProverOutput =
            serde_json::from_slice(&output).context("invalid proof host result")?;
        ensure!(
            output.mode == "prove"
                && output.kind == "groth16"
                && output.image_id == hex::encode(input.profile.guest_image_id)
                && output.profile_id == hex::encode(input.profile.digest()?)
                && output.relation == input.statement.relation()? as u8
                && output.journal == hex::encode(input.statement.abi_bytes()),
            "proof host did not prove the requested statement"
        );
        let seal = hex::decode(output.seal)?;
        ensure!(
            seal.len() == 260 && seal[..4] == [0x73, 0xc4, 0x57, 0xba],
            "unrecognized proof seal"
        );
        let verified = verified_seal(config, input, receipt, &lock).await?;
        ensure!(verified == seal, "receipt and emitted proof seal differ");
        ensure!(
            config.oom_events()? == oom_before,
            "proof memory limit was exceeded; receipt was not accepted"
        );
        Ok(seal)
    }
}

async fn verified_seal(
    config: &ProofConfig,
    input: &ProofInput,
    receipt: &Path,
    lock: &File,
) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(receipt)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "proof receipt is not a regular file"
    );
    let mut bytes = Vec::new();
    tokio::fs::File::open(receipt)
        .await?
        .take(16_385)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= 16_384,
        "proof receipt exceeds its reserved storage"
    );
    let mut verify = command(config, lock)?;
    verify
        .arg("verify")
        .arg(hex::encode(input.statement.abi_bytes()));
    let result = bounded_run(
        verify,
        &bytes,
        4096,
        std::time::Duration::from_secs(VERIFY_TIMEOUT_SECS),
    )
    .await?;
    let result: serde_json::Value = serde_json::from_slice(&result)?;
    ensure!(
        result["verified"] == true
            && result["kind"] == "groth16"
            && result["imageId"] == hex::encode(input.profile.guest_image_id)
            && result["journalHash"] == hex::encode(input.statement.digest()),
        "public receipt verification failed"
    );
    let seal = hex::decode(
        result["seal"]
            .as_str()
            .context("verified receipt has no Groth16 seal")?,
    )?;
    ensure!(
        seal.len() == 260 && seal[..4] == [0x73, 0xc4, 0x57, 0xba],
        "unrecognized verified proof seal"
    );
    Ok(seal)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lock_directory() -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("proof-lock-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir).unwrap();
        dir
    }
    #[tokio::test]
    #[ignore = "subprocess fixture invoked by the proof restart tests"]
    async fn inherited_lock_fixture() {
        let dir = PathBuf::from(
            std::env::var_os("VIBEFACE_DELIVERY_LOCK_TEST_DIR").expect("fixture directory"),
        );
        if std::env::var_os("VIBEFACE_DELIVERY_LOCK_TEST_BEFORE").is_some() {
            crate::config::private_write(&dir.join("ready.json"), b"{}").unwrap();
            std::future::pending::<()>().await;
            return;
        }
        let lock = process_lock(&dir.join("proof.receipt")).unwrap();
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-c","import os,subprocess,sys\nfd=int(os.environ['VIBEFACE_DELIVERY_PROOF_LOCK_FD'])\nos.fstat(fd)\np=subprocess.Popen(['/bin/sleep','120'],pass_fds=(fd,))\nopen(sys.argv[1],'w').write(str(p.pid))\np.wait()"])
            .arg(dir.join("descendant.pid")).env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).process_group(0);
        inherit_lock(&mut command, &lock);
        let host = command.spawn().unwrap();
        let pid = host.id().unwrap();
        let _group = ProcessGroup(pid);
        for _ in 0..200 {
            if dir.join("descendant.pid").exists() {
                let child = std::fs::read_to_string(dir.join("descendant.pid")).unwrap();
                if let Ok(descendant) = child.parse::<u32>() {
                    crate::config::private_write(
                        &dir.join("ready.json"),
                        &serde_json::to_vec(
                            &serde_json::json!({"host":pid,"descendant":descendant}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    std::future::pending::<()>().await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("proof lock fixture did not start its descendant");
    }
    async fn fixture(dir: &Path, before: bool) -> tokio::process::Child {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "delivery::proof::tests::inherited_lock_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("VIBEFACE_DELIVERY_LOCK_TEST_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if before {
            command.env("VIBEFACE_DELIVERY_LOCK_TEST_BEFORE", "1");
        }
        let mut child = command.spawn().unwrap();
        for _ in 0..200 {
            if dir.join("ready.json").exists() {
                return child;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "proof lock fixture exited before readiness"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("proof lock fixture readiness timed out");
    }
    #[tokio::test]
    async fn a_crash_before_the_lock_does_not_block_a_fresh_proof() {
        let dir = lock_directory();
        let mut node = fixture(&dir, true).await;
        node.start_kill().unwrap();
        node.wait().await.unwrap();
        let lock = process_lock(&dir.join("proof.receipt")).unwrap();
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn sigkill_keeps_the_proof_locked_until_the_last_inheriting_descendant_exits() {
        let dir = lock_directory();
        let receipt = dir.join("proof.receipt");
        let mut node = fixture(&dir, false).await;
        let ready: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("ready.json")).unwrap()).unwrap();
        let host = ready["host"].as_u64().unwrap() as u32;
        let group = ProcessGroup(host);
        assert!(process_lock(&receipt).unwrap_err().is::<Busy>());
        node.start_kill().unwrap();
        node.wait().await.unwrap();
        assert!(process_lock(&receipt).unwrap_err().is::<Busy>());
        std::fs::write(&receipt, b"public receipt fixture").unwrap();
        assert!(
            process_lock(&receipt).unwrap_err().is::<Busy>(),
            "an existing receipt must not bypass an active proof lock"
        );
        unsafe {
            libc::kill(host as i32, libc::SIGKILL);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            process_lock(&receipt).unwrap_err().is::<Busy>(),
            "the proof descendant must retain the lock after its host exits"
        );
        drop(group);
        let mut released = false;
        for _ in 0..100 {
            if let Ok(lock) = process_lock(&receipt) {
                drop(lock);
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            released,
            "proof lock was not released after the process group exited"
        );
        assert_eq!(std::fs::read(&receipt).unwrap(), b"public receipt fixture");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn an_error_exit_releases_the_inherited_lock() {
        let dir = lock_directory();
        let receipt = dir.join("proof.receipt");
        let lock = process_lock(&receipt).unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        inherit_lock(&mut command, &lock);
        let mut child = command.spawn().unwrap();
        let _group = ProcessGroup(child.id().unwrap());
        drop(lock);
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        let lock = process_lock(&receipt).unwrap();
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn confined_groth16_shim_requires_an_inherited_regular_lock_descriptor() {
        fn shim() -> tokio::process::Command {
            let mut command = tokio::process::Command::new("/bin/bash");
            command
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../proofs/delivery/confined-gnark.sh"),
                )
                .arg("--version")
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .stdin(Stdio::null());
            command
        }
        let dir = lock_directory();
        let lock = process_lock(&dir.join("proof.receipt")).unwrap();
        let mut inherited = shim();
        inherit_lock(&mut inherited, &lock);
        let output = inherited.output().await.unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            output.stdout,
            b"Vibeface confined RISC Zero Groth16 runner v2025-04-03.1\n"
        );
        let output = shim()
            .env("VIBEFACE_DELIVERY_PROOF_LOCK_FD", "2147483647")
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(output.stderr, b"invalid inherited proof lock descriptor\n");
        let directory = File::open(&dir).unwrap();
        let mut invalid = shim();
        inherit_lock(&mut invalid, &directory);
        let output = invalid.output().await.unwrap();
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(output.stderr, b"invalid inherited proof lock descriptor\n");
        drop(directory);
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn proof_request_closes_stdin_before_waiting_for_output() {
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command
            .args(["-c", "import sys; data=sys.stdin.buffer.read(); assert data == b'x' * 131072; sys.stdout.buffer.write(b'complete')"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let output = bounded_run(
            command,
            &vec![b'x'; 131_072],
            8,
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(output, b"complete");
    }
    #[tokio::test]
    async fn excessive_output_fails_without_waiting_for_more() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "printf 'too-long'"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        assert!(
            bounded_run(command, &[], 3, std::time::Duration::from_secs(2))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn timeout_terminates_the_whole_process_group() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".artifacts")
            .join("proof-supervisor-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.pid", uuid::Uuid::new_v4()));
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "sleep 120 & printf '%s' \"$!\" > \"$1\"; wait", "--"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        assert!(
            bounded_run(command, &[], 64, std::time::Duration::from_millis(300))
                .await
                .is_err()
        );
        let pid: i32 = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let mut terminated = false;
        for _ in 0..20 {
            let state = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                terminated = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        std::fs::remove_file(path).unwrap();
        assert!(terminated, "timed-out proof task left a live descendant");
    }
}
