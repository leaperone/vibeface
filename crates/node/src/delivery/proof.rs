use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vibeface_delivery::{ProofInput, ProofProfile, verify_relation};
use zeroize::Zeroizing;

pub const INFO_TIMEOUT_SECS: u64 = 30;
pub const PROVE_TIMEOUT_SECS: u64 = 7200;
pub const VERIFY_TIMEOUT_SECS: u64 = 120;

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
fn command(config: &ProofConfig) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(&config.program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
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
        let mut identify = command(config)?;
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
            let seal = verified_seal(config, input, receipt).await?;
            ensure!(
                config.oom_events()? == oom_before,
                "proof memory limit was exceeded; receipt was not accepted"
            );
            return Ok(seal);
        }
        let mut prove = command(config)?;
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
        let verified = verified_seal(config, input, receipt).await?;
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
    let mut verify = command(config)?;
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
