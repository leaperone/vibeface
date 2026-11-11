use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vibeface_bridge::platform::privatefs;
use vibeface_delivery::{ProofInput, ProofProfile, verify_relation};
use zeroize::Zeroizing;

pub const INFO_TIMEOUT_SECS: u64 = 30;
pub const PROVE_TIMEOUT_SECS: u64 = 7200;
pub const VERIFY_TIMEOUT_SECS: u64 = 120;

#[cfg(unix)]
pub(crate) const DIAGNOSTIC_FD: &str = "VIBEFACE_DELIVERY_DIAGNOSTIC_FD";
const DIAGNOSTIC_LIMIT: u64 = 32 * 1024;

pub(crate) enum ProofDiagnostics<'a> {
    Registration(Option<&'a Diagnostics>),
    Order {
        data_dir: &'a Path,
        node_id: Option<String>,
        order_id: &'a str,
        window: u16,
    },
}

#[derive(Clone, Copy)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "non-Unix diagnostics have no sink constructor")
)]
enum DiagnosticFormat {
    RegistrationV1,
    OrderJobV2 { source: bool },
}

pub(crate) struct Diagnostics {
    file: File,
    format: DiagnosticFormat,
    target: std::sync::OnceLock<[u64; 2]>,
}
impl Diagnostics {
    pub(crate) fn open(data_dir: &Path, intent: &str) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let intent_id = uuid::Uuid::parse_str(intent).ok()?;
            if intent_id.to_string() != intent {
                return None;
            }
            let private = data_dir.join("delivery/private");
            for path in [
                data_dir.to_owned(),
                data_dir.join("delivery"),
                private.clone(),
            ] {
                let metadata = std::fs::symlink_metadata(path).ok()?;
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o077 != 0
                {
                    return None;
                }
            }
            let file = std::fs::OpenOptions::new()
                .append(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(private.join(format!("key-diagnostics-{intent}.jsonl")))
                .ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o600
                || metadata.len() != 0
                || metadata.nlink() != 1
            {
                return None;
            }
            Some(Self {
                file,
                format: DiagnosticFormat::RegistrationV1,
                target: Default::default(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (data_dir, intent);
            None
        }
    }

    fn claim_order(data_dir: &Path, statement: &vibeface_delivery::Statement) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::{
                fd::{AsRawFd, FromRawFd},
                unix::fs::{MetadataExt, OpenOptionsExt},
            };
            let relation = statement.relation().ok()? as u8;
            if !matches!(relation, 2 | 3) {
                return None;
            }
            let mut directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
                .open(data_dir)
                .ok()?;
            for component in [
                None,
                Some(c"delivery"),
                Some(c"private"),
                Some(c"proof-diagnostics"),
            ] {
                if let Some(component) = component {
                    let fd = unsafe {
                        libc::openat(
                            directory.as_raw_fd(),
                            component.as_ptr(),
                            libc::O_RDONLY
                                | libc::O_DIRECTORY
                                | libc::O_NOFOLLOW
                                | libc::O_CLOEXEC
                                | libc::O_NONBLOCK,
                        )
                    };
                    if fd < 0 {
                        return None;
                    }
                    directory = unsafe { File::from_raw_fd(fd) };
                }
                let metadata = directory.metadata().ok()?;
                if !metadata.is_dir()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o7777 != 0o700
                {
                    return None;
                }
            }
            let name = std::ffi::CString::new(format!("{}.jsonl", hex::encode(statement.digest())))
                .ok()?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_APPEND
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK,
                    0o600,
                )
            };
            if fd < 0 {
                return None;
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata().ok()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o600
                || metadata.len() != 0
                || metadata.nlink() != 1
            {
                return None;
            }
            Some(Self {
                file,
                format: DiagnosticFormat::OrderJobV2 {
                    source: relation == 2,
                },
                target: Default::default(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (data_dir, statement);
            None
        }
    }

    fn activate_order(
        &self,
        scope: &ProofDiagnostics<'_>,
        config: &ProofConfig,
        input: &ProofInput,
    ) -> Option<()> {
        let ProofDiagnostics::Order {
            node_id: Some(node),
            order_id,
            window,
            ..
        } = scope
        else {
            return None;
        };
        for value in [node.as_str(), *order_id] {
            if value.len() != 64
                || !value
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return None;
            }
        }
        let prover = diagnostic_tool_hash(&config.program)?;
        let r0vm = diagnostic_tool_hash(&config.r0vm)?;
        let target = diagnostic_group_identity(&config.cgroup);
        if let Some(target) = target {
            let _ = self.target.set(target);
        }
        let current = diagnostic_group().and_then(|root| diagnostic_group_identity(&root));
        let job = serde_json::json!({"v":2,"kind":"job","node":node,"order":order_id,
            "relation":input.statement.relation().ok()? as u8,"window":window,
            "job":hex::encode(input.statement.digest()),"profile":hex::encode(input.profile.digest().ok()?)});
        let tools = serde_json::json!({"v":2,"kind":"tools","prover":prover,"r0vm":r0vm,
            "target_cgroup":target,"core_same_target":target.is_some() && target == current});
        self.write(&job);
        self.write(&tools);
        Some(())
    }

    fn write(&self, value: &serde_json::Value) {
        use std::io::Write;
        let Ok(mut bytes) = serde_json::to_vec(value) else {
            return;
        };
        bytes.push(b'\n');
        let Ok(metadata) = self.file.metadata() else {
            return;
        };
        if bytes.len() > 512 || metadata.len().saturating_add(bytes.len() as u64) > DIAGNOSTIC_LIMIT
        {
            let _ = self.file.set_len(DIAGNOSTIC_LIMIT + 1);
            return;
        }
        if (&self.file).write(&bytes).ok() != Some(bytes.len()) {
            let _ = self.file.set_len(DIAGNOSTIC_LIMIT + 1);
        }
    }
}

fn diagnostic_tool_hash(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use sha2::{Digest, Sha256};
        use std::{
            io::Read,
            os::unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let before = file.metadata().ok()?;
        if !before.is_file() || before.mode() & 0o022 != 0 {
            return None;
        }
        let mut hash = Sha256::new();
        let mut remaining = before.len();
        let mut buffer = [0; 65536];
        while remaining > 0 {
            let capacity = remaining.min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..capacity]).ok()?;
            if count == 0 {
                return None;
            }
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        let after = file.metadata().ok()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
            before.mode(),
            before.uid(),
            before.gid(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
            after.mode(),
            after.uid(),
            after.gid(),
        ) {
            return None;
        }
        Some(hex::encode(hash.finalize()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn diagnostic_time() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
            return None;
        }
        let value = u64::try_from(time.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(time.tv_nsec).ok()?)?;
        (value <= i64::MAX as u64).then_some(value)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn diagnostic_group() -> Option<PathBuf> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
}

fn diagnostic_group_identity(path: &Path) -> Option<[u64; 2]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        metadata
            .is_dir()
            .then_some([metadata.dev(), metadata.ino()])
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn diagnostic_values(path: &Path) -> Option<[u64; 5]> {
    let values = |name: &str| -> Option<std::collections::HashMap<String, u64>> {
        let mut result = std::collections::HashMap::new();
        for line in std::fs::read_to_string(path.join(name)).ok()?.lines() {
            let (key, value) = line.split_once(' ')?;
            let value = value.parse::<u64>().ok()?;
            if value > i64::MAX as u64 || result.insert(key.to_owned(), value).is_some() {
                return None;
            }
        }
        Some(result)
    };
    let memory = values("memory.events")?;
    let pids = values("pids.events")?;
    Some([
        *pids.get("max")?,
        *memory.get("max")?,
        *memory.get("oom")?,
        *memory.get("oom_kill")?,
        *memory.get("oom_group_kill")?,
    ])
}

#[derive(Default)]
struct CommandIo {
    spawned: std::sync::atomic::AtomicBool,
    written: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
    eof: std::sync::atomic::AtomicBool,
    bytes: std::sync::atomic::AtomicUsize,
    waited: std::sync::atomic::AtomicBool,
    timed_out: std::sync::atomic::AtomicBool,
}
impl CommandIo {
    fn new() -> Self {
        Self {
            bytes: std::sync::atomic::AtomicUsize::new(usize::MAX),
            ..Default::default()
        }
    }
    fn json(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let count = self.bytes.load(Relaxed);
        serde_json::json!([
            self.spawned.load(Relaxed),
            self.written.load(Relaxed),
            self.closed.load(Relaxed),
            self.eof.load(Relaxed),
            (count != usize::MAX).then_some(count),
            self.waited.load(Relaxed),
            self.timed_out.load(Relaxed)
        ])
    }
}

fn diagnostic_counters(phase: &str) -> Option<serde_json::Value> {
    if !matches!(
        phase,
        "key_preproof" | "proof_info" | "proof_prove" | "proof_verify" | "register_tx"
    ) {
        return None;
    }
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    let cgroup = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    let read = |name: &str| -> Option<std::collections::HashMap<String, u64>> {
        let mut result = std::collections::HashMap::new();
        for line in std::fs::read_to_string(cgroup.join(name)).ok()?.lines() {
            let (key, value) = line.split_once(' ')?;
            let value = value.parse::<u64>().ok()?;
            if value > i64::MAX as u64 || result.insert(key.to_owned(), value).is_some() {
                return None;
            }
        }
        Some(result)
    };
    let memory = read("memory.events")?;
    let pids = read("pids.events")?;
    Some(
        serde_json::json!({"pids_max":pids.get("max")?, "memory_max":memory.get("max")?,
        "oom":memory.get("oom")?, "oom_kill":memory.get("oom_kill")?, "oom_group_kill":memory.get("oom_group_kill")?}),
    )
}

pub(crate) struct DiagnosticStage<'a> {
    sink: Option<&'a Diagnostics>,
    phase: &'static str,
    command: &'static str,
    success: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    errno: Option<i32>,
    class: &'static str,
    io: Option<serde_json::Value>,
    mode: &'static str,
}
impl<'a> DiagnosticStage<'a> {
    pub(crate) fn new(
        sink: Option<&'a Diagnostics>,
        phase: &'static str,
        command: &'static str,
    ) -> Self {
        let stage = Self {
            sink,
            phase,
            command,
            success: false,
            exit_code: None,
            signal: None,
            errno: None,
            class: "stage_error",
            io: sink
                .filter(|sink| matches!(sink.format, DiagnosticFormat::OrderJobV2 { .. }))
                .filter(|_| matches!(phase, "proof_info" | "proof_prove" | "proof_verify"))
                .map(|_| CommandIo::new().json()),
            mode: "not_started",
        };
        stage.emit("begin");
        stage
    }
    fn emit(&self, edge: &str) {
        if let Some(sink) = self.sink {
            if matches!(sink.format, DiagnosticFormat::OrderJobV2 { .. }) {
                let group = diagnostic_group();
                let identity = group.as_deref().and_then(diagnostic_group_identity);
                let values = group.as_deref().and_then(diagnostic_values);
                sink.write(&serde_json::json!({"v":2,"kind":"event","w":"core","c":self.command,"p":self.phase,"e":edge,
                    "t":diagnostic_time(),"x":self.exit_code,"s":self.signal,"n":self.errno,
                    "f":if edge == "error" { Some(self.class) } else { None },"r":null,"k":false,
                    "g":identity,"q":values,"i":self.io,"a":(self.phase == "proof_job").then_some(self.mode)}));
                return;
            }
            sink.write(&serde_json::json!({"v":1,"writer":"core","command":self.command,"phase":self.phase,"edge":edge,
                "exit_code":self.exit_code,"signal":self.signal,"errno":self.errno,
                "failure_class":if edge == "error" { Some(self.class) } else { None },
                "worker":null,"cleanup_requested":false,"counters":diagnostic_counters(self.phase)}));
        }
    }
    pub(crate) fn ok(&mut self) {
        self.success = true;
    }
    fn io_error(&mut self, error: &std::io::Error) {
        self.errno = error.raw_os_error();
    }
    fn observe(&self, phase: &str, io: &CommandIo, status: Option<std::process::ExitStatus>) {
        let Some(sink) = self
            .sink
            .filter(|sink| matches!(sink.format, DiagnosticFormat::OrderJobV2 { .. }))
        else {
            return;
        };
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.and_then(|status| status.signal())
        };
        #[cfg(not(unix))]
        let signal: Option<i32> = None;
        sink.write(&serde_json::json!({"v":2,"kind":"event","w":"core","c":self.command,"p":phase,"e":"observed",
            "t":diagnostic_time(),"x":status.and_then(|status| status.code()),"s":signal,"n":null,"f":null,
            "r":null,"k":false,"g":null,"q":null,"i":io.json(),"a":null}));
    }
    fn activity(&self, edge: &str) {
        let Some(sink) = self
            .sink
            .filter(|sink| matches!(sink.format, DiagnosticFormat::OrderJobV2 { source: true }))
            .filter(|_| self.phase == "proof_prove")
        else {
            return;
        };
        let group = diagnostic_group();
        let identity = group.as_deref().and_then(diagnostic_group_identity);
        let group = group.filter(|_| identity.as_ref() == sink.target.get() && identity.is_some());
        let total = |value: &str| {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| *value <= i64::MAX as u64)
        };
        let cpu = group
            .as_ref()
            .and_then(|root| std::fs::read_to_string(root.join("cpu.stat")).ok());
        let stat = |key: &str| {
            cpu.as_ref().and_then(|text| {
                text.lines().find_map(|line| {
                    let (name, value) = line.split_once(' ')?;
                    (name == key).then(|| total(value)).flatten()
                })
            })
        };
        let pressure = |file: &str, category: &str| {
            group
                .as_ref()
                .and_then(|root| std::fs::read_to_string(root.join(file)).ok())
                .and_then(|text| {
                    text.lines().find_map(|line| {
                        let mut fields = line.split_whitespace();
                        if fields.next()? != category {
                            return None;
                        }
                        fields.find_map(|field| field.strip_prefix("total=").and_then(total))
                    })
                })
        };
        let values = [
            stat("usage_usec"),
            stat("user_usec"),
            stat("system_usec"),
            pressure("cpu.pressure", "some"),
            pressure("memory.pressure", "some"),
            pressure("memory.pressure", "full"),
            pressure("io.pressure", "some"),
        ];
        sink.write(&serde_json::json!({"v":2,"kind":"source_activity","e":edge,"t":diagnostic_time(),"g":identity,"q":values}));
    }
}
impl Drop for DiagnosticStage<'_> {
    fn drop(&mut self) {
        self.emit(if self.success { "ok" } else { "error" });
    }
}

#[cfg(unix)]
fn inherit_diagnostics(command: &mut tokio::process::Command, diagnostics: &Diagnostics) {
    use std::os::fd::AsRawFd;
    let fd = diagnostics.file.as_raw_fd();
    command.env(DIAGNOSTIC_FD, fd.to_string());
    if matches!(diagnostics.format, DiagnosticFormat::OrderJobV2 { .. }) {
        command.env("VIBEFACE_DELIVERY_DIAGNOSTIC_FORMAT", "order-job-v2");
    }
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
}

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
    let directory = receipt
        .parent()
        .context("proof receipt has no private directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "the proof receipt directory must not be a link"
    );
    privatefs::ensure_private(directory, "the proof receipt directory")?;
    let path = directory.join("prover.lock");
    let file = privatefs::open_or_create(&path)?;
    ensure!(
        file.metadata()?.is_file(),
        "proof lock must be a private regular file"
    );
    privatefs::ensure_private(&path, "the proof lock")?;
    // The prover inherits this descriptor, so it is moved above the standard streams: a
    // prover started with fewer than three open would otherwise be handed the lock as its
    // own standard input.
    #[cfg(unix)]
    let file = {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        // SAFETY: the descriptor is open and fcntl only duplicates it.
        let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        ensure!(
            duplicate >= 0,
            "proof lock descriptor could not be reserved"
        );
        // SAFETY: the duplicate is a fresh descriptor this File now owns alone.
        unsafe { File::from_raw_fd(duplicate) }
    };
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Busy.into()),
        Err(error) => Err(error.into()),
    }
}

/// Hands the prover the open lock so the trusted host and its local descendants keep holding it
/// if this Node is killed.
#[cfg(unix)]
fn inherit_lock(command: &mut tokio::process::Command, lock: &File) {
    use std::os::fd::AsRawFd as _;
    let fd = lock.as_raw_fd();
    command.env("VIBEFACE_DELIVERY_PROOF_LOCK_FD", fd.to_string());
    // SAFETY: the closure only calls fcntl on a descriptor this process owns, which is
    // async-signal-safe and allocates nothing.
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
        // No prover runs anywhere else: `command` refuses a family that cannot carry the Linux
        // cgroup memory limit paid proving is authorized against.
        #[cfg(windows)]
        let _ = self.0;
    }
}
fn command(
    config: &ProofConfig,
    lock: &File,
    diagnostics: Option<&Diagnostics>,
) -> Result<tokio::process::Command> {
    // `check_memory_limit` refuses to authorize paid proving without a Linux cgroup memory
    // limit, so no prover is ever started anywhere else. Saying so here keeps the cgroup
    // membership and the inherited lock below on the one family that has them.
    ensure!(
        cfg!(target_os = "linux"),
        "paid proving requires a Linux cgroup memory limit"
    );
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
        inherit_lock(&mut command, lock);
        if let Some(diagnostics) = diagnostics {
            inherit_diagnostics(&mut command, diagnostics);
        }
    }
    #[cfg(windows)]
    let _ = (lock, diagnostics);
    Ok(command)
}
async fn bounded_run(
    mut command: tokio::process::Command,
    input: &[u8],
    limit: usize,
    timeout: std::time::Duration,
    mut diagnostic: Option<&mut DiagnosticStage<'_>>,
) -> Result<Vec<u8>> {
    use std::sync::atomic::Ordering::Relaxed;
    let io = CommandIo::new();
    if let Some(stage) = diagnostic.as_deref() {
        stage.activity("begin");
    }
    let mut child = command.spawn().inspect_err(|error| {
        if let Some(stage) = diagnostic.as_deref_mut() {
            stage.class = "spawn";
            stage.io_error(error);
            if stage.io.is_some() {
                stage.io = Some(io.json());
            }
            stage.activity("end");
        }
    })?;
    io.spawned.store(true, Relaxed);
    let observer = diagnostic.as_deref();
    if let Some(stage) = observer {
        stage.observe("process_spawn", &io, None);
    }
    let _group = ProcessGroup(child.id().context("proof task has no process identity")?);
    let mut stdin = child.stdin.take().context("proof stdin unavailable")?;
    let mut stdout = child.stdout.take().context("proof stdout unavailable")?;
    let mut class = "io";
    let mut status = None;
    let run = async {
        let writer = async {
            stdin.write_all(input).await?;
            io.written.store(true, Relaxed);
            stdin.shutdown().await?;
            drop(stdin);
            io.closed.store(true, Relaxed);
            if let Some(stage) = observer {
                stage.observe("stdin_closed", &io, None);
            }
            Ok::<_, anyhow::Error>(())
        };
        let reader = async {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            io.bytes.store(bytes.len(), Relaxed);
            io.eof.store(bytes.len() <= limit, Relaxed);
            if let Some(stage) = observer {
                stage.observe("stdout_complete", &io, None);
            }
            if bytes.len() > limit {
                class = "output_limit";
            }
            ensure!(bytes.len() <= limit, "proof task output exceeds its bound");
            Ok::<_, anyhow::Error>(bytes)
        };
        let (_, output) = tokio::try_join!(writer, reader)?;
        let observed = child.wait().await?;
        status = Some(observed);
        io.waited.store(true, Relaxed);
        if let Some(stage) = observer {
            stage.observe("process_wait", &io, Some(observed));
        }
        if !observed.success() {
            class = "worker_exit";
        }
        ensure!(
            observed.success(),
            "proof task failed; order remains unresolved"
        );
        Ok::<_, anyhow::Error>(output)
    };
    let result = tokio::time::timeout(timeout, run).await;
    io.timed_out.store(result.is_err(), Relaxed);
    if let Some(stage) = observer {
        stage.activity("end");
    }
    if let Some(stage) = diagnostic {
        if stage.io.is_some() {
            stage.io = Some(io.json());
        }
        stage.class = if result.is_err() { "timeout" } else { class };
        if let Some(status) = status {
            stage.exit_code = status.code();
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                stage.signal = status.signal();
            }
            if stage.signal.is_some() {
                stage.class = "worker_signal";
            }
        }
        if let Ok(Err(error)) = &result
            && let Some(error) = error.downcast_ref::<std::io::Error>()
        {
            stage.io_error(error);
        }
    }
    result.context("proof task exceeded its execution deadline")?
}
impl ProofRunner {
    pub(crate) async fn prove(
        &self,
        config: &ProofConfig,
        input: &ProofInput,
        receipt: &Path,
        diagnostics: ProofDiagnostics<'_>,
    ) -> Result<Vec<u8>> {
        let registration = match &diagnostics {
            ProofDiagnostics::Registration(value) => *value,
            _ => None,
        };
        let mut preflight = DiagnosticStage::new(registration, "proof_preflight", "key");
        let _slot = self
            .slot
            .try_acquire()
            .context("proof queue is busy; no charge was submitted")?;
        let lock = process_lock(receipt)?;
        let order_sink = match &diagnostics {
            ProofDiagnostics::Order { data_dir, .. } => {
                Diagnostics::claim_order(data_dir, &input.statement)
            }
            _ => None,
        };
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
        preflight.ok();
        drop(preflight);
        let order_sink =
            order_sink.filter(|sink| sink.activate_order(&diagnostics, config, input).is_some());
        let diagnostics = registration.or(order_sink.as_ref());
        let mut job = DiagnosticStage::new(order_sink.as_ref(), "proof_job", "prove");
        let mut info_stage = DiagnosticStage::new(diagnostics, "proof_info", "info");
        let mut identify = command(config, &lock, diagnostics)?;
        identify.arg("info");
        let info = bounded_run(
            identify,
            &[],
            4096,
            std::time::Duration::from_secs(INFO_TIMEOUT_SECS),
            Some(&mut info_stage),
        )
        .await?;
        info_stage.class = "result_decode";
        let info: ProverInfo =
            serde_json::from_slice(&info).context("invalid proof host identification")?;
        ensure!(
            info.image_id == hex::encode(input.profile.guest_image_id)
                && info.schema_hash == hex::encode(input.profile.statement_schema_hash),
            "proof host image or schema mismatch"
        );
        info_stage.ok();
        drop(info_stage);
        if receipt.try_exists()? {
            job.mode = "receipt_reconcile";
            let seal = verified_seal(config, input, receipt, &lock, diagnostics).await?;
            ensure!(
                config.oom_events()? == oom_before,
                "proof memory limit was exceeded; receipt was not accepted"
            );
            job.ok();
            return Ok(seal);
        }
        job.mode = "prove";
        let mut prove_stage = DiagnosticStage::new(diagnostics, "proof_prove", "prove");
        let mut prove = command(config, &lock, diagnostics)?;
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
            Some(&mut prove_stage),
        )
        .await?;
        prove_stage.class = "result_decode";
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
        prove_stage.ok();
        drop(prove_stage);
        let verified = verified_seal(config, input, receipt, &lock, diagnostics).await?;
        ensure!(verified == seal, "receipt and emitted proof seal differ");
        ensure!(
            config.oom_events()? == oom_before,
            "proof memory limit was exceeded; receipt was not accepted"
        );
        job.ok();
        Ok(seal)
    }
}

async fn verified_seal(
    config: &ProofConfig,
    input: &ProofInput,
    receipt: &Path,
    lock: &File,
    diagnostics: Option<&Diagnostics>,
) -> Result<Vec<u8>> {
    let mut stage = DiagnosticStage::new(diagnostics, "proof_verify", "verify");
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
    let mut verify = command(config, lock, diagnostics)?;
    verify
        .arg("verify")
        .arg(hex::encode(input.statement.abi_bytes()));
    let result = bounded_run(
        verify,
        &bytes,
        4096,
        std::time::Duration::from_secs(VERIFY_TIMEOUT_SECS),
        Some(&mut stage),
    )
    .await?;
    stage.class = "receipt_check";
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
    stage.ok();
    Ok(seal)
}

#[cfg(all(test, unix))]
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
    /// Whether the lock can be taken again. Another test thread's fork holds a copy of a lock
    /// descriptor until its child execs, so a released lock can read busy for a moment.
    async fn released(receipt: &Path) -> bool {
        for _ in 0..100 {
            match process_lock(receipt) {
                Ok(_) => return true,
                Err(error) => assert!(error.is::<Busy>(), "{error}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
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
        assert!(
            released(&receipt).await,
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
        assert!(
            released(&receipt).await,
            "proof lock was not released after the error exit"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn confined_groth16_shim_requires_an_inherited_regular_lock_descriptor() {
        fn shim() -> tokio::process::Command {
            let mut command = tokio::process::Command::new("/bin/bash");
            command
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../crates/delivery/proof/confined-gnark.sh"),
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
            None,
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
            bounded_run(command, &[], 3, std::time::Duration::from_secs(2), None)
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
            bounded_run(
                command,
                &[],
                64,
                std::time::Duration::from_millis(300),
                None
            )
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

    fn diagnostic_fixture() -> (Diagnostics, PathBuf, String) {
        use std::os::unix::fs::OpenOptionsExt;
        let root = lock_directory();
        let private = root.join("delivery/private");
        crate::config::private_dir(&root.join("delivery")).unwrap();
        crate::config::private_dir(&private).unwrap();
        let intent = uuid::Uuid::new_v4().to_string();
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(private.join(format!("key-diagnostics-{intent}.jsonl")))
            .unwrap();
        (Diagnostics::open(&root, &intent).unwrap(), root, intent)
    }

    #[test]
    fn diagnostic_sink_only_opens_the_exact_owner_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (sink, root, intent) = diagnostic_fixture();
        let path = root.join(format!("delivery/private/key-diagnostics-{intent}.jsonl"));
        assert!(Diagnostics::open(&root, &uuid::Uuid::new_v4().to_string()).is_none());
        assert!(Diagnostics::open(&root, "../different").is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Diagnostics::open(&root, &intent).is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let moved = path.with_extension("controlled");
        std::fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert!(Diagnostics::open(&root, &intent).is_none());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(Diagnostics::open(&root, &intent).is_none());
        std::fs::remove_dir(&path).unwrap();
        let fifo = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(Diagnostics::open(&root, &intent).is_none());
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn order_diagnostics_are_opt_in_exclusive_and_refuse_nonregular_tools() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
        let root = lock_directory();
        let stored = super::super::store::tests::paid_order(super::super::types::Role::Sender);
        let statement = vibeface_delivery::Statement::window(
            &stored.order.unsigned,
            0,
            vibeface_delivery::Relation::Source,
        )
        .unwrap();
        assert!(Diagnostics::claim_order(&root, &statement).is_none());
        assert!(!root.join("delivery").exists());
        let private = root.join("delivery/private/proof-diagnostics");
        for path in [
            root.join("delivery"),
            root.join("delivery/private"),
            private.clone(),
        ] {
            crate::config::private_dir(&path).unwrap();
        }
        let sink = Diagnostics::claim_order(&root, &statement).unwrap();
        let path = private.join(format!("{}.jsonl", hex::encode(statement.digest())));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert!(Diagnostics::claim_order(&root, &statement).is_none());
        drop(sink);
        std::fs::remove_dir(&private).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        let moved = root.join("tool");
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&moved)
            .unwrap();
        assert_eq!(
            diagnostic_tool_hash(&moved).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        symlink(&moved, &path).unwrap();
        assert!(Diagnostics::claim_order(&root, &statement).is_none());
        assert!(diagnostic_tool_hash(&path).is_none());
        std::fs::remove_file(&path).unwrap();
        let fifo = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(Diagnostics::claim_order(&root, &statement).is_none());
        assert!(diagnostic_tool_hash(&path).is_none());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&moved, &path).unwrap();
        assert!(Diagnostics::claim_order(&root, &statement).is_none());
        std::fs::remove_file(&path).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Diagnostics::claim_order(&root, &statement).is_none());
        std::fs::set_permissions(&moved, std::fs::Permissions::from_mode(0o620)).unwrap();
        assert!(diagnostic_tool_hash(&moved).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn order_io_records_real_status_and_only_source_activity_endpoints() {
        for source in [false, true] {
            for (script, timed_out, code, signal) in [
                ("printf ok", false, Some(0), None),
                ("exit 137", false, Some(137), None),
                ("kill -KILL $$", false, None, Some(9)),
                ("sleep 120", true, None, None),
            ] {
                let (mut sink, root, intent) = diagnostic_fixture();
                sink.format = DiagnosticFormat::OrderJobV2 { source };
                let mut stage = DiagnosticStage::new(Some(&sink), "proof_prove", "prove");
                let mut command = tokio::process::Command::new("/bin/sh");
                command
                    .args(["-c", script])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .process_group(0);
                let result = bounded_run(
                    command,
                    b"",
                    64,
                    std::time::Duration::from_millis(if timed_out { 100 } else { 2000 }),
                    Some(&mut stage),
                )
                .await;
                if code == Some(0) {
                    assert_eq!(result.unwrap(), b"ok");
                    stage.ok();
                } else {
                    assert!(result.is_err());
                }
                drop(stage);
                let records: Vec<serde_json::Value> = std::fs::read_to_string(
                    root.join(format!("delivery/private/key-diagnostics-{intent}.jsonl")),
                )
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
                assert_eq!(
                    records
                        .iter()
                        .filter(|value| value["kind"] == "source_activity")
                        .count(),
                    if source { 2 } else { 0 }
                );
                let last = records.last().unwrap();
                assert_eq!(last["v"], 2);
                assert_eq!(last["x"], serde_json::json!(code));
                assert_eq!(last["s"], serde_json::json!(signal));
                assert_eq!(last["i"][6], timed_out);
                if timed_out {
                    assert_eq!(last["i"][4], serde_json::Value::Null);
                    assert_eq!(last["i"][5], false);
                }
                drop(sink);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn diagnostic_status_uses_real_spawn_errno_exit_and_signal() {
        for (script, errno, code, signal) in [
            ("exit 137", None, Some(137), None),
            ("kill -KILL $$", None, None, Some(9)),
            ("exit 0", Some(libc::EAGAIN), None, None),
            ("exit 0", Some(libc::ENOMEM), None, None),
        ] {
            let (sink, root, intent) = diagnostic_fixture();
            let mut stage = DiagnosticStage::new(Some(&sink), "proof_prove", "prove");
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .args(["-c", script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .process_group(0);
            if let Some(errno) = errno {
                unsafe {
                    command.pre_exec(move || Err(std::io::Error::from_raw_os_error(errno)));
                }
            }
            let error = bounded_run(
                command,
                &[],
                64,
                std::time::Duration::from_secs(2),
                Some(&mut stage),
            )
            .await
            .unwrap_err();
            if errno.is_none() {
                assert!(error.to_string().contains("proof task failed"));
            }
            drop(stage);
            let path = root.join(format!("delivery/private/key-diagnostics-{intent}.jsonl"));
            let events: Vec<serde_json::Value> = std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let last = events.last().unwrap();
            assert_eq!(last["exit_code"], serde_json::json!(code));
            assert_eq!(last["signal"], serde_json::json!(signal));
            assert_eq!(last["errno"], serde_json::json!(errno));
            assert_eq!(last["edge"], "error");
            drop(sink);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn diagnostic_descriptor_survives_exec_without_changing_standard_streams() {
        for order in [false, true] {
            let (mut sink, root, _) = diagnostic_fixture();
            if order {
                sink.format = DiagnosticFormat::OrderJobV2 { source: false };
            }
            let mut command = tokio::process::Command::new("/bin/bash");
            command.args(["-c", "exec /usr/bin/python3 -c 'import fcntl,os,stat,sys; fd=int(os.environ[\"VIBEFACE_DELIVERY_DIAGNOSTIC_FD\"]); assert stat.S_ISREG(os.fstat(fd).st_mode); assert fcntl.fcntl(fd,fcntl.F_GETFL)&os.O_APPEND; assert os.environ.get(\"VIBEFACE_DELIVERY_DIAGNOSTIC_FORMAT\",\"registration-v1\")==sys.argv[1]; assert sys.stdin.buffer.read()==b\"synthetic stdin\"; sys.stdout.buffer.write(b\"unchanged\")' \"$1\"", "--"])
            .arg(if order { "order-job-v2" } else { "registration-v1" })
            .env_remove("VIBEFACE_DELIVERY_DIAGNOSTIC_FORMAT")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .kill_on_drop(true).process_group(0);
            inherit_diagnostics(&mut command, &sink);
            let output = bounded_run(
                command,
                b"synthetic stdin",
                64,
                std::time::Duration::from_secs(2),
                None,
            )
            .await
            .unwrap();
            assert_eq!(output, b"unchanged");
            drop(sink);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
