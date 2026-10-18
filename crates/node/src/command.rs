use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::watch,
};

pub const MAX_TIMEOUT_SECS: u64 = 300;
const MAX_COMMAND_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const COMMAND_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/Library/Developer/CommandLineTools/usr/bin:/Applications/Xcode.app/Contents/Developer/usr/bin:/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(not(target_os = "macos"))]
const COMMAND_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub timeout_secs: u64,
}

impl CommandSpec {
    pub fn validate(&self) -> Result<(), CommandError> {
        if self.command.trim().is_empty()
            || self.command.len() > MAX_COMMAND_BYTES
            || self.command.contains('\0')
        {
            return Err(CommandError::known(
                "command must contain 1 to 32768 bytes without NUL",
            ));
        }
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return Err(CommandError::known(
                "command timeout must be between 1 and 300 seconds",
            ));
        }
        if !self.cwd.is_absolute() {
            return Err(CommandError::known("command cwd must be absolute"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub uncertain: bool,
    pub message: String,
}

impl CommandError {
    fn known(message: impl Into<String>) -> Self {
        Self {
            uncertain: false,
            message: message.into(),
        }
    }

    fn unknown(message: impl Into<String>) -> Self {
        Self {
            uncertain: true,
            message: message.into(),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}

pub async fn execute(
    spec: &CommandSpec,
    mut cancel: watch::Receiver<bool>,
) -> Result<CommandOutput, CommandError> {
    spec.validate()?;
    let cwd = std::fs::canonicalize(&spec.cwd)
        .map_err(|error| CommandError::known(format!("cannot resolve command cwd: {error}")))?;
    if !cwd.is_dir() || cwd.parent().is_none() {
        return Err(CommandError::known(
            "command cwd must be a directory other than the filesystem root",
        ));
    }
    if *cancel.borrow_and_update() {
        return Err(CommandError::known("command cancelled before starting"));
    }

    let mut command = sandboxed_command(&cwd)?;
    command
        .arg("-c")
        .arg(&spec.command)
        .current_dir(&cwd)
        .env_clear()
        .env("PATH", COMMAND_PATH)
        .env("LANG", "C.UTF-8")
        .env("HOME", &cwd)
        .env("TMPDIR", &cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| CommandError::known(format!("cannot start sandboxed command: {error}")))?;
    let mut group = ProcessGroup(
        child
            .id()
            .ok_or_else(|| CommandError::unknown("command has no process id"))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CommandError::unknown("command has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CommandError::unknown("command has no stderr pipe"))?;
    let observation = async {
        let (status, stdout, stderr) = tokio::join!(child.wait(), capture(stdout), capture(stderr));
        Ok::<_, io::Error>((status?, stdout?, stderr?))
    };
    let result = tokio::select! {
        result = observation => result.map_err(|error| CommandError::unknown(format!("cannot observe command result: {error}"))),
        _ = tokio::time::sleep(Duration::from_secs(spec.timeout_secs)) => Err(CommandError::unknown("command timed out; its effects require reconciliation")),
        _ = cancelled(&mut cancel) => Err(CommandError::unknown("command cancelled after starting; its effects require reconciliation")),
    };
    match result {
        Ok((status, stdout, stderr)) => {
            if group.exists().map_err(|error| {
                CommandError::unknown(format!("cannot inspect command process group: {error}"))
            })? {
                let cleaned = group.terminate(&mut child).await;
                return Err(CommandError::unknown(if cleaned {
                    "command left background processes; they were stopped and its effects require reconciliation"
                } else {
                    "command left background processes and their cleanup could not be confirmed"
                }));
            }
            group.0 = 0;
            Ok(CommandOutput {
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
                stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
                truncated: stdout.truncated || stderr.truncated,
            })
        }
        Err(mut error) => {
            if !group.terminate(&mut child).await {
                error
                    .message
                    .push_str("; process-group cleanup could not be confirmed");
            }
            Err(error)
        }
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn capture(mut stream: impl AsyncRead + Unpin) -> io::Result<Captured> {
    let mut output = Captured {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut chunk = [0_u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(output);
        }
        let retained = count.min(MAX_OUTPUT_BYTES - output.bytes.len());
        output.bytes.extend_from_slice(&chunk[..retained]);
        output.truncated |= retained < count;
    }
}

struct ProcessGroup(u32);

impl ProcessGroup {
    fn signal(&self, signal: i32) -> io::Result<()> {
        if self.0 == 0 {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        #[cfg(unix)]
        {
            if unsafe { libc::kill(-(self.0 as i32), signal) } == 0 {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        #[cfg(not(unix))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process groups unavailable",
        ))
    }

    fn exists(&self) -> io::Result<bool> {
        match self.signal(0) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn terminate(&mut self, child: &mut Child) -> bool {
        let _ = self.signal(libc::SIGKILL);
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
            Ok(Ok(_))
        ) {
            return false;
        }
        for _ in 0..20 {
            match self.exists() {
                Ok(false) => {
                    self.0 = 0;
                    return true;
                }
                Err(_) => return false,
                Ok(true) => {
                    let _ = self.signal(libc::SIGKILL);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = self.signal(libc::SIGKILL);
    }
}

#[cfg(target_os = "macos")]
fn sandboxed_command(cwd: &Path) -> Result<Command, CommandError> {
    let cwd = cwd
        .to_str()
        .filter(|value| !value.chars().any(char::is_control))
        .ok_or_else(|| {
            CommandError::known("sandbox cwd must be UTF-8 without control characters")
        })?;
    let cwd = cwd.replace('\\', "\\\\").replace('"', "\\\"");
    let profile = format!(
        r#"(version 1)
(deny default)
(allow process-fork process-exec)
(allow file-read-metadata)
(allow signal (target same-sandbox))
(deny process-info* sysctl*)
(allow process-info* (target self))
(allow sysctl-read
  (sysctl-name "hw.pagesize" "hw.pagesize32" "hw.pagesize_compat"
    "hw.ncpu" "hw.logicalcpu" "hw.physicalcpu" "kern.osrelease" "kern.ostype" "kern.osversion"))
(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid SYS_ptrace SYS_posix_spawn))
(allow network-outbound (remote ip))
(allow network-bind network-inbound (local ip))
(allow mach-lookup
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.mDNSResponder"))
(allow file-read* (literal "/") (subpath "/private/var/select")
  (literal "/private/etc/resolv.conf") (literal "/private/etc/hosts")
  (literal "/private/etc/services") (literal "/private/etc/protocols")
  (literal "/private/etc/ssl/openssl.cnf") (literal "/private/etc/ssl/cert.pem")
  (subpath "/private/etc/ssl/certs"))
(allow file-read* file-map-executable
  (subpath "/bin") (subpath "/sbin") (subpath "/usr/bin") (subpath "/usr/sbin")
  (subpath "/usr/lib") (subpath "/usr/share") (subpath "/System/Library")
  (subpath "/private/var/db/dyld") (subpath "/Library/Developer/CommandLineTools")
  (subpath "/Applications/Xcode.app")
  (subpath "/opt/homebrew/Cellar") (subpath "/opt/homebrew/opt")
  (subpath "/opt/homebrew/bin") (subpath "/opt/homebrew/lib")
  (subpath "/opt/homebrew/share") (subpath "/usr/local/bin") (subpath "/usr/local/lib"))
(allow file-read* file-write* (literal "/dev/null") (literal "/dev/zero"))
(allow file-read* (literal "/dev/random") (literal "/dev/urandom"))
(allow file-read* file-write* file-map-executable (subpath "{cwd}"))
"#
    );
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &profile, "/bin/sh"]);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn sandboxed_command(cwd: &Path) -> Result<Command, CommandError> {
    let sandbox = linux::Sandbox::prepare(cwd).map_err(|error| {
        CommandError::known(format!(
            "required Landlock/seccomp sandbox is unavailable: {error}"
        ))
    })?;
    let mut command = Command::new("/bin/sh");
    // All descriptors and BPF instructions are prepared before fork; entry only invokes syscalls.
    unsafe {
        command.pre_exec(move || sandbox.enter());
    }
    Ok(command)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandboxed_command(_: &Path) -> Result<Command, CommandError> {
    Err(CommandError::known(
        "sandboxed commands are unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        ffi::CString,
        mem::size_of,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
    };

    const ALL_FS: u64 = (1 << 16) - 1;
    const READ: u64 = (1 << 0) | (1 << 2) | (1 << 3);
    const FILE_READ: u64 = 1 << 2;
    const DEVICE_RW: u64 = FILE_READ | (1 << 1) | (1 << 14) | (1 << 15);

    #[repr(C)]
    struct Ruleset {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }
    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    pub struct Sandbox {
        ruleset: OwnedFd,
        filter: Vec<libc::sock_filter>,
    }

    impl Sandbox {
        pub fn prepare(cwd: &Path) -> io::Result<Self> {
            if unsafe { libc::geteuid() } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "command sandbox requires an unprivileged node user",
                ));
            }
            // ABI 6 adds signal and abstract-socket scoping.
            let abi = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<u8>(),
                    0,
                    1,
                )
            };
            if abi < 6 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Landlock ABI 6 or newer is required",
                ));
            }
            let attr = Ruleset {
                handled_access_fs: ALL_FS,
                handled_access_net: 0,
                scoped: 3,
            };
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    &attr,
                    size_of::<Ruleset>(),
                    0,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            let add = |path: &Path, access| -> io::Result<()> {
                let path = CString::new(path.as_os_str().as_bytes())?;
                let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                let parent = unsafe { OwnedFd::from_raw_fd(fd) };
                let attr = PathBeneath {
                    allowed_access: access,
                    parent_fd: parent.as_raw_fd(),
                };
                if unsafe {
                    libc::syscall(
                        libc::SYS_landlock_add_rule,
                        ruleset.as_raw_fd(),
                        1,
                        &attr,
                        0,
                    )
                } < 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            };
            add(cwd, ALL_FS)?;
            for directory in [
                "/bin",
                "/sbin",
                "/usr/bin",
                "/usr/sbin",
                "/usr/lib",
                "/usr/lib64",
                "/usr/share",
                "/lib",
                "/lib64",
                "/usr/local/bin",
                "/usr/local/lib",
            ] {
                let path = Path::new(directory);
                if path.exists() {
                    add(path, READ)?;
                }
            }
            for path in [
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/nsswitch.conf",
                "/etc/services",
                "/etc/protocols",
                "/etc/ld.so.cache",
                "/dev/random",
                "/dev/urandom",
            ] {
                let path = Path::new(path);
                if path.exists() {
                    add(path, FILE_READ)?;
                }
            }
            for path in [
                "/etc/ssl/certs",
                "/etc/pki/tls/certs",
                "/etc/pki/ca-trust/extracted",
            ] {
                let path = Path::new(path);
                if path.exists() {
                    add(path, READ)?;
                }
            }
            for path in ["/dev/null", "/dev/zero"] {
                add(Path::new(path), DEVICE_RW)?;
            }
            Ok(Self {
                ruleset,
                filter: syscall_filter()?,
            })
        }

        pub fn enter(&self) -> io::Result<()> {
            let program = libc::sock_fprog {
                len: self.filter.len() as u16,
                filter: self.filter.as_ptr().cast_mut(),
            };
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0
                || unsafe {
                    libc::syscall(
                        libc::SYS_landlock_restrict_self,
                        self.ruleset.as_raw_fd(),
                        0,
                    )
                } < 0
                || unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) } < 0
                || unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    fn syscall_filter() -> io::Result<Vec<libc::sock_filter>> {
        #[cfg(target_arch = "x86_64")]
        let architecture = 0xc000003e;
        #[cfg(target_arch = "aarch64")]
        let architecture = 0xc00000b7;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "seccomp architecture is unsupported",
        ));
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let statement = |code, k| libc::sock_filter {
                code,
                jt: 0,
                jf: 0,
                k,
            };
            let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
            let mut filter = vec![
                statement(0x20, 4),
                jump(0x15, architecture, 1, 0),
                statement(0x06, libc::SECCOMP_RET_KILL_PROCESS),
                statement(0x20, 0),
                jump(0x35, 0x40000000, 0, 1),
                statement(0x06, libc::SECCOMP_RET_KILL_PROCESS),
            ];
            for syscall in [libc::SYS_socket, libc::SYS_socketpair] {
                filter.extend([
                    jump(0x15, syscall as u32, 0, 3),
                    // seccomp_data.args[0] is the socket family, at byte offset 16.
                    statement(0x20, 16),
                    jump(0x15, libc::AF_UNIX as u32, 0, 1),
                    statement(0x06, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
                    statement(0x20, 0),
                ]);
            }
            let forbidden = [
                libc::SYS_setsid,
                libc::SYS_setpgid,
                libc::SYS_ptrace,
                libc::SYS_unshare,
                libc::SYS_setns,
                libc::SYS_process_vm_readv,
                libc::SYS_process_vm_writev,
                libc::SYS_pidfd_getfd,
                libc::SYS_keyctl,
                libc::SYS_request_key,
                libc::SYS_add_key,
                libc::SYS_shmget,
                libc::SYS_shmat,
                libc::SYS_shmctl,
                libc::SYS_shmdt,
                libc::SYS_msgget,
                libc::SYS_msgsnd,
                libc::SYS_msgrcv,
                libc::SYS_msgctl,
                libc::SYS_semget,
                libc::SYS_semop,
                libc::SYS_semtimedop,
                libc::SYS_semctl,
                libc::SYS_io_uring_setup,
                libc::SYS_io_uring_enter,
                libc::SYS_io_uring_register,
                libc::SYS_fchmod,
                libc::SYS_fchmodat,
                libc::SYS_fchown,
                libc::SYS_fchownat,
                libc::SYS_utimensat,
                libc::SYS_setxattr,
                libc::SYS_lsetxattr,
                libc::SYS_fsetxattr,
                libc::SYS_removexattr,
                libc::SYS_lremovexattr,
                libc::SYS_fremovexattr,
                libc::SYS_getxattr,
                libc::SYS_lgetxattr,
                libc::SYS_fgetxattr,
                libc::SYS_listxattr,
                libc::SYS_llistxattr,
                libc::SYS_flistxattr,
                // fchmodat2 is syscall 452 on both supported Linux architectures.
                452,
            ];
            for syscall in forbidden {
                filter.push(jump(0x15, syscall as u32, 0, 1));
                filter.push(statement(
                    0x06,
                    libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                ));
            }
            filter.push(jump(0x15, libc::SYS_clone3 as u32, 0, 1));
            filter.push(statement(
                0x06,
                libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
            ));
            #[cfg(target_arch = "x86_64")]
            for syscall in [
                libc::SYS_chmod,
                libc::SYS_chown,
                libc::SYS_lchown,
                libc::SYS_utime,
                libc::SYS_utimes,
                libc::SYS_futimesat,
            ] {
                filter.push(jump(0x15, syscall as u32, 0, 1));
                filter.push(statement(
                    0x06,
                    libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                ));
            }
            filter.push(statement(0x06, libc::SECCOMP_RET_ALLOW));
            Ok(filter)
        }
    }

    #[cfg(all(test, any(target_arch = "x86_64", target_arch = "aarch64")))]
    #[test]
    fn seccomp_denies_access_to_host_sysv_shared_memory() {
        struct Segment(i32);
        impl Drop for Segment {
            fn drop(&mut self) {
                unsafe {
                    libc::shmctl(self.0, libc::IPC_RMID, std::ptr::null_mut());
                }
            }
        }

        let segment =
            Segment(unsafe { libc::shmget(libc::IPC_PRIVATE, 4096, libc::IPC_CREAT | 0o600) });
        assert!(segment.0 >= 0, "cannot create private test shared memory");
        let memory = unsafe { libc::shmat(segment.0, std::ptr::null(), 0) };
        assert_ne!(
            memory as isize, -1,
            "host must be able to attach its test segment"
        );
        unsafe {
            memory.cast::<u8>().write(0x5a);
        }
        assert_eq!(unsafe { memory.cast::<u8>().read() }, 0x5a);
        assert_eq!(unsafe { libc::shmdt(memory) }, 0);

        let filter = syscall_filter().unwrap();
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr().cast_mut(),
        };
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "cannot fork the seccomp test probe");
        if child == 0 {
            let installed = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0
                && unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) } == 0;
            if !installed {
                unsafe {
                    libc::_exit(120);
                }
            }
            let attached = unsafe { libc::shmat(segment.0, std::ptr::null(), 0) };
            let denied =
                attached as isize == -1 && unsafe { *libc::__errno_location() } == libc::EPERM;
            unsafe {
                libc::_exit(if denied { 0 } else { 121 });
            }
        }
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(child, &mut status, 0) };
            if result == child {
                break;
            }
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EINTR));
        }
        assert_eq!(
            status, 0,
            "seccomp must reject attaching same-UID 0600 host shared memory"
        );
        let memory = unsafe { libc::shmat(segment.0, std::ptr::null(), 0) };
        assert_ne!(memory as isize, -1);
        assert_eq!(unsafe { memory.cast::<u8>().read() }, 0x5a);
        assert_eq!(unsafe { libc::shmdt(memory) }, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        root: PathBuf,
        cwd: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let location = std::env::current_dir().unwrap();
            let repository = location
                .ancestors()
                .find(|path| path.join(".git").exists())
                .unwrap_or(&location);
            let root = repository.join(".artifacts").join(format!(
                "c{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let cwd = root.join("workspace");
            std::fs::create_dir_all(&cwd).unwrap();
            Self {
                root: std::fs::canonicalize(root).unwrap(),
                cwd: std::fs::canonicalize(cwd).unwrap(),
            }
        }

        fn spec(&self, command: impl Into<String>) -> CommandSpec {
            CommandSpec {
                command: command.into(),
                cwd: self.cwd.clone(),
                timeout_secs: 5,
            }
        }

        async fn run(&self, command: impl Into<String>) -> Result<CommandOutput, CommandError> {
            let (_sender, cancel) = watch::channel(false);
            execute(&self.spec(command), cancel).await
        }

        fn supported(&self) -> bool {
            match sandboxed_command(&self.cwd) {
                Ok(_) => true,
                Err(error) => {
                    eprintln!("functional command test requires host sandbox support: {error}");
                    false
                }
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn success_and_nonzero_preserve_real_output() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let result = fixture
            .run("printf hello; printf warning >&2; printf saved > result")
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "hello");
        assert_eq!(result.stderr, "warning");
        assert!(!result.truncated);
        assert_eq!(std::fs::read(fixture.cwd.join("result")).unwrap(), b"saved");
        let result = fixture.run("printf no >&2; exit 7").await.unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.stderr, "no");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn system_developer_tools_remain_usable() {
        let fixture = Fixture::new();
        let output = fixture.run("git --version").await.unwrap();
        assert_eq!(output.exit_code, Some(0), "{}", output.stderr);
        assert!(output.stdout.starts_with("git version "));
    }

    #[tokio::test]
    async fn outside_reads_writes_and_symlink_escape_are_denied() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let outside = fixture.root.join("outside");
        std::fs::write(&outside, b"private-fixture").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, fixture.cwd.join("escape")).unwrap();
        for path in [&outside, &fixture.cwd.join("escape")] {
            let result = fixture
                .run(format!("/bin/cat {}", quote(path)))
                .await
                .unwrap();
            assert_ne!(result.exit_code, Some(0));
            assert!(!result.stdout.contains("private-fixture"));
            let result = fixture
                .run(format!("printf overwritten > {}", quote(path)))
                .await
                .unwrap();
            assert_ne!(result.exit_code, Some(0));
            assert_eq!(std::fs::read(&outside).unwrap(), b"private-fixture");
        }
        let result = fixture
            .run(format!(
                "printf created > {}",
                quote(&fixture.root.join("created"))
            ))
            .await
            .unwrap();
        assert_ne!(result.exit_code, Some(0));
        assert!(!fixture.root.join("created").exists());
    }

    #[tokio::test]
    async fn environment_contains_only_explicit_safe_values() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let result = fixture.run("/usr/bin/env").await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        for variable in result.stdout.lines() {
            let (name, _) = variable.split_once('=').unwrap();
            assert!(
                ["PATH", "LANG", "HOME", "TMPDIR", "PWD", "SHLVL", "_"].contains(&name),
                "unexpected inherited variable {name}"
            );
        }
        assert!(
            result
                .stdout
                .lines()
                .any(|line| line == format!("HOME={}", fixture.cwd.display()))
        );
    }

    #[tokio::test]
    async fn drains_both_streams_with_bounded_output() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let result = fixture.run("(/usr/bin/yes x | /usr/bin/head -c 1200000) & /usr/bin/yes y | /usr/bin/head -c 1200000 >&2; wait").await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.truncated);
        assert_eq!(result.stdout.len(), MAX_OUTPUT_BYTES);
        assert_eq!(result.stderr.len(), MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn cancellation_stops_the_group_and_keeps_effects_uncertain() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let spec = fixture.spec("printf ready > started; /bin/sleep 2; printf late > late");
        let (sender, cancel) = watch::channel(false);
        let execution = tokio::spawn(async move { execute(&spec, cancel).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.cwd.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        sender.send(true).unwrap();
        let error = execution.await.unwrap().unwrap_err();
        assert!(error.uncertain);
        assert!(error.message.contains("cancelled"));
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert!(!fixture.cwd.join("late").exists());
    }

    #[tokio::test]
    async fn timeout_stops_the_group_and_never_claims_success() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let mut spec = fixture.spec("/bin/sleep 2; printf late > late");
        spec.timeout_secs = 1;
        let (_sender, cancel) = watch::channel(false);
        let error = execute(&spec, cancel).await.unwrap_err();
        assert!(error.uncertain);
        assert!(error.message.contains("timed out"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!fixture.cwd.join("late").exists());
    }

    #[tokio::test]
    async fn background_children_cannot_report_a_completed_command() {
        let fixture = Fixture::new();
        if !fixture.supported() {
            return;
        }
        let error = fixture
            .run("(/bin/sleep 2; printf late > late) >/dev/null 2>&1 & exit 0")
            .await
            .unwrap_err();
        assert!(error.uncertain);
        assert!(error.message.contains("background processes"));
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert!(!fixture.cwd.join("late").exists());
    }

    #[tokio::test]
    async fn cancelled_before_start_does_not_execute() {
        let fixture = Fixture::new();
        let (_sender, cancel) = watch::channel(true);
        let error = execute(&fixture.spec("printf unexpected > result"), cancel)
            .await
            .unwrap_err();
        assert!(!error.uncertain);
        assert!(!fixture.cwd.join("result").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn missing_sandbox_capability_fails_before_any_command_effect() {
        let fixture = Fixture::new();
        if sandboxed_command(&fixture.cwd).is_ok() {
            return;
        }
        let error = fixture.run("printf unexpected > result").await.unwrap_err();
        assert!(!error.uncertain);
        assert!(error.message.contains("sandbox is unavailable"));
        assert!(!fixture.cwd.join("result").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fake_credential_sentinel() {
        if std::env::var_os("VIBEFACE_FAKE_SENTINEL").is_some() {
            std::thread::sleep(Duration::from_secs(15));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_escape_probe() {
        use std::os::unix::process::CommandExt;
        let Ok(pid) = std::fs::read_to_string(".command-probe-pid") else {
            return;
        };
        let mut name = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            pid.trim().parse().unwrap(),
        ];
        let mut buffer = [0_u8; 8192];
        let mut length = buffer.len();
        let result = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                name.len() as u32,
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(
            result, -1,
            "sandbox must deny reading another process environment"
        );
        assert!(
            !buffer
                .windows(b"VIBEFACE_FAKE_SENTINEL".len())
                .any(|part| part == b"VIBEFACE_FAKE_SENTINEL")
        );
        let socket = std::fs::read_to_string(".command-probe-socket").unwrap();
        assert!(std::os::unix::net::UnixStream::connect(socket).is_err());
        assert_eq!(unsafe { libc::setpgid(0, 0) }, -1);
        let fork = unsafe { libc::fork() };
        assert!(fork >= 0);
        if fork == 0 {
            let denied = unsafe { libc::setsid() } == -1;
            unsafe {
                libc::_exit(if denied { 0 } else { 1 });
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(fork, &mut status, 0) }, fork);
        assert_eq!(status, 0, "sandbox must deny descendant setsid");
        let spawned = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .status();
        assert!(
            spawned.is_err(),
            "posix_spawn must not create a process in another group"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn process_environment_and_group_escapes_are_denied() {
        let fixture = Fixture::new();
        let executable = std::env::current_exe().unwrap();
        let mut sentinel = std::process::Command::new(&executable)
            .args(["--exact", "command::tests::fake_credential_sentinel"])
            .env_clear()
            .env("VIBEFACE_FAKE_SENTINEL", "public-test-marker")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(
            fixture.cwd.join(".command-probe-pid"),
            sentinel.id().to_string(),
        )
        .unwrap();
        let socket = fixture.root.join("host.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::write(
            fixture.cwd.join(".command-probe-socket"),
            socket.to_str().unwrap(),
        )
        .unwrap();
        let probe = fixture.cwd.join("probe");
        std::fs::copy(executable, &probe).unwrap();
        let result = fixture
            .run(format!(
                "{} --exact command::tests::sandbox_escape_probe --nocapture",
                quote(&probe)
            ))
            .await;
        let _ = sentinel.kill();
        sentinel.wait().unwrap();
        let output = result.unwrap();
        assert_eq!(
            output.exit_code,
            Some(0),
            "probe failed: {} {}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("1 passed"));
    }

    #[test]
    fn spec_rejects_unknown_fields_and_unbounded_commands() {
        assert!(
            serde_json::from_str::<CommandSpec>(
                r#"{"command":"pwd","cwd":"/","timeout_secs":1,"secret":"no"}"#
            )
            .is_err()
        );
        let spec = CommandSpec {
            command: "pwd".into(),
            cwd: PathBuf::from("relative"),
            timeout_secs: 1,
        };
        assert!(!spec.validate().unwrap_err().uncertain);
        assert!(
            CommandSpec {
                cwd: PathBuf::from("/"),
                timeout_secs: 0,
                ..spec
            }
            .validate()
            .is_err()
        );
    }
}
