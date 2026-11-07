//! Bounded, read-only discovery of an already running Core owned by this account.
//!
//! Only an executable with Vibeface's fixed basename is considered. Raw process metadata stays
//! local to this function and only `--data-dir` values leave it.

use std::{io, path::PathBuf};

#[cfg(any(target_os = "linux", windows))]
const MAX_PROCESSES: usize = 8192;
const MAX_COMMAND_LINE: usize = 64 * 1024;

fn parse_node_args(args: &[std::ffi::OsString]) -> Option<PathBuf> {
    if args.len() > 256 {
        return None;
    }
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--data-dir" {
            let path = args.get(index + 1).map(PathBuf::from)?;
            return path.is_absolute().then_some(path);
        }
        if let Some(value) = args[index]
            .to_str()
            .and_then(|s| s.strip_prefix("--data-dir="))
        {
            let path = PathBuf::from(value);
            return path.is_absolute().then_some(path);
        }
        index += 1;
    }
    None
}

#[cfg(target_os = "linux")]
pub fn running_core_data_dirs() -> io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt as _;
    use std::{ffi::OsString, fs};
    let uid = unsafe { libc::geteuid() };
    let mut found = Vec::new();
    let entries = fs::read_dir("/proc")?;
    for entry in entries.flatten().take(MAX_PROCESSES) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let base = entry.path();
        let Ok(metadata) = fs::metadata(&base) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != uid {
            continue;
        }
        let Ok(exe) = fs::read_link(base.join("exe")) else {
            continue;
        };
        if exe.file_name().and_then(|s| s.to_str()) != Some("vibeface-core") {
            continue;
        }
        let Ok(command) = fs::File::open(base.join("cmdline")) else {
            continue;
        };
        use std::io::Read as _;
        let mut bytes = Vec::new();
        if command
            .take((MAX_COMMAND_LINE + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() > MAX_COMMAND_LINE {
            continue;
        }
        let args = bytes
            .split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| OsString::from_vec(part.to_vec()))
            .collect::<Vec<_>>();
        // The proc directory may have been recycled while metadata was read; recheck its pid.
        if fs::read_to_string(base.join("stat"))
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<u32>().ok())
            != Some(pid)
        {
            continue;
        }
        if let Some(path) = parse_node_args(&args) {
            found.push(path);
        }
    }
    Ok(found)
}

#[cfg(target_os = "macos")]
pub fn running_core_data_dirs() -> io::Result<Vec<PathBuf>> {
    // macOS command lines contain argv followed by environment strings in KERN_PROCARGS2. The
    // implementation below uses libproc to enumerate same-user processes, then sysctl only for
    // executable-matched PIDs; it never calls ps or splits a rendered command line.
    macos::running_core_data_dirs()
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use libc::{c_int, c_void};
    use std::{ffi::CStr, os::unix::ffi::OsStrExt as _};
    #[repr(C)]
    struct BsdInfo {
        bytes: [u8; 256],
    }
    struct Scrub(Vec<u8>);
    impl Drop for Scrub {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int)
        -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }
    pub fn running_core_data_dirs() -> io::Result<Vec<PathBuf>> {
        const PROC_ALL_PIDS: u32 = 1;
        const PROC_PIDTBSDINFO: c_int = 3;
        const KERN_PROCARGS2: c_int = 49;
        const CTL_KERN: c_int = 1;
        const MAX_PIDS: usize = 8192;
        let mut pids = vec![0_i32; MAX_PIDS];
        let bytes = unsafe {
            proc_listpids(
                PROC_ALL_PIDS,
                0,
                pids.as_mut_ptr().cast(),
                (pids.len() * 4) as c_int,
            )
        };
        if bytes < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = (bytes as usize / 4).min(MAX_PIDS);
        let uid = unsafe { libc::geteuid() };
        let mut found = Vec::new();
        for pid in pids.into_iter().take(count).filter(|pid| *pid > 0) {
            let mut info = BsdInfo { bytes: [0; 256] };
            let got = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDTBSDINFO,
                    0,
                    info.bytes.as_mut_ptr().cast(),
                    256,
                )
            };
            if got < 24 || u32::from_ne_bytes(info.bytes[20..24].try_into().unwrap()) != uid {
                continue;
            }
            let mut path = vec![0_i8; 4096];
            let n = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
            if n <= 0 {
                continue;
            }
            let exe = unsafe { CStr::from_ptr(path.as_ptr()) }.to_bytes();
            if std::path::Path::new(std::ffi::OsStr::from_bytes(exe))
                .file_name()
                .and_then(|s| s.to_str())
                != Some("vibeface-core")
            {
                continue;
            }
            let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
            let mut size = 0_usize;
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    std::ptr::null_mut(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
                || size < 4
                || size > MAX_COMMAND_LINE
            {
                continue;
            }
            let mut raw = Scrub(vec![0_u8; size]);
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    raw.0.as_mut_ptr().cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
            {
                continue;
            }
            raw.0.truncate(size);
            if let Some(args) = parse_kern_proc_args(&raw.0)
                && let Some(path) = super::parse_node_args(&args)
            {
                found.push(path);
            }
        }
        Ok(found)
    }
}

#[cfg(target_os = "macos")]
fn parse_kern_proc_args(raw: &[u8]) -> Option<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStringExt as _;
    if raw.len() < 4 || raw.len() > MAX_COMMAND_LINE {
        return None;
    }
    let argc = i32::from_ne_bytes(raw[..4].try_into().ok()?);
    if !(1..=256).contains(&argc) {
        return None;
    }
    let mut cursor = 4;
    cursor += raw[cursor..].iter().position(|byte| *byte == 0)? + 1;
    while cursor < raw.len() && raw[cursor] == 0 {
        cursor += 1;
    }
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        if cursor >= raw.len() {
            return None;
        }
        let end = cursor + raw[cursor..].iter().position(|byte| *byte == 0)?;
        args.push(std::ffi::OsString::from_vec(raw[cursor..end].to_vec()));
        cursor = end + 1;
    }
    Some(args)
}

#[cfg(windows)]
pub fn running_core_data_dirs() -> io::Result<Vec<PathBuf>> {
    windows::running_core_data_dirs()
}

#[cfg(windows)]
mod windows {
    use super::*;
    use serde::Deserialize;
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt as _, OsStringExt as _},
        process::Command,
    };
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
    #[derive(Deserialize)]
    struct Proc {
        owner_sid: String,
        current_sid: String,
        name: String,
        command_line: Option<String>,
    }
    #[derive(Deserialize)]
    struct ProcessList {
        processes: Vec<Proc>,
    }

    fn system_powershell() -> io::Result<PathBuf> {
        let mut buffer = vec![0_u16; 32_768];
        let length =
            unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if length == 0 || length >= buffer.len() {
            return Err(io::Error::last_os_error());
        }
        let system = PathBuf::from(OsString::from_wide(&buffer[..length]));
        Ok(system.join("WindowsPowerShell/v1.0/powershell.exe"))
    }

    pub fn running_core_data_dirs() -> io::Result<Vec<PathBuf>> {
        let ps = system_powershell()?;
        let script = "$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $n=0; $items=@(); Get-CimInstance Win32_Process | Where-Object { $_.Name -ieq 'vibeface-core.exe' } | ForEach-Object { if ($n -lt 64) { $p=$_; $o=$p.GetOwnerSid(); if ($o.Sid -eq $sid) { $n++; $line=[string]$p.CommandLine; if ($line.Length -gt 4096) { $line=$line.Substring(0,4096) }; $items+=@{owner_sid=$o.Sid;current_sid=$sid;name=$p.Name;command_line=$line} } } }; [pscustomobject]@{processes=$items} | ConvertTo-Json -Compress -Depth 2";
        let output = run_powershell_bounded(&ps, script, 256 * 1024)?;
        if !output.status.success() || output.stdout.len() > 256 * 1024 {
            return Err(io::Error::other("process metadata unavailable"));
        }
        let rows: ProcessList = serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
        let mut found = Vec::new();
        for row in rows.processes.into_iter().take(MAX_PROCESSES) {
            if row.owner_sid != row.current_sid
                || !row.name.eq_ignore_ascii_case("vibeface-core.exe")
            {
                continue;
            }
            let Some(command_line) = row.command_line else {
                continue;
            };
            let wide: Vec<u16> = std::ffi::OsStr::new(&command_line)
                .encode_wide()
                .chain(Some(0))
                .collect();
            let mut argc = 0;
            let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
            if argv.is_null() {
                continue;
            }
            let args = unsafe { std::slice::from_raw_parts(argv, argc.clamp(0, 256) as usize) }
                .iter()
                .map(|arg| {
                    let mut n = 0;
                    while unsafe { *(*arg).add(n) } != 0 {
                        n += 1;
                    }
                    OsString::from_wide(unsafe { std::slice::from_raw_parts(*arg, n) })
                })
                .collect::<Vec<_>>();
            unsafe {
                LocalFree(argv.cast());
            }
            if let Some(path) = parse_node_args(&args) {
                found.push(path);
            }
        }
        Ok(found)
    }

    pub fn command_line_args(line: &str) -> Option<Vec<OsString>> {
        if line.len() > MAX_COMMAND_LINE {
            return None;
        }
        let wide: Vec<u16> = std::ffi::OsStr::new(line)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut argc = 0;
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
        if argv.is_null() {
            return None;
        }
        let count = argc.clamp(0, 256) as usize;
        let args = unsafe { std::slice::from_raw_parts(argv, count) }
            .iter()
            .map(|arg| {
                let mut n = 0;
                while unsafe { *(*arg).add(n) } != 0 {
                    n += 1;
                }
                OsString::from_wide(unsafe { std::slice::from_raw_parts(*arg, n) })
            })
            .collect();
        unsafe {
            LocalFree(argv.cast());
        }
        Some(args)
    }

    fn run_powershell_bounded(
        path: &PathBuf,
        script: &str,
        limit: usize,
    ) -> io::Result<std::process::Output> {
        use std::{
            io::Read as _,
            process::Stdio,
            thread,
            time::{Duration, Instant},
        };
        let mut child = Command::new(path)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut pipe = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("process metadata output unavailable"))?;
        let reader = thread::spawn(move || -> io::Result<(Vec<u8>, bool)> {
            let mut output = Vec::new();
            let mut chunk = [0_u8; 4096];
            let mut oversized = false;
            loop {
                let count = pipe.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                if output.len().saturating_add(count) > limit {
                    oversized = true;
                } else if !oversized {
                    output.extend_from_slice(&chunk[..count]);
                }
            }
            Ok((output, oversized))
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(error);
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process metadata query timed out",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait()?;
        let (stdout, oversized) = reader
            .join()
            .map_err(|_| io::Error::other("process metadata reader stopped unexpectedly"))??;
        if oversized {
            return Err(io::Error::other(
                "process metadata exceeded its output bound",
            ));
        }
        Ok(std::process::Output {
            status,
            stdout,
            stderr: Vec::new(),
        })
    }

    pub fn read_only_powershell(script: &str, output_limit: usize) -> io::Result<Vec<u8>> {
        let path = system_powershell()?;
        let output = run_powershell_bounded(&path, script, output_limit)?;
        if !output.status.success() {
            return Err(io::Error::other("metadata query failed"));
        }
        Ok(output.stdout)
    }
}

#[cfg(windows)]
pub fn command_line_args(line: &str) -> Option<Vec<std::ffi::OsString>> {
    windows::command_line_args(line)
}

#[cfg(windows)]
pub fn read_only_powershell(script: &str, output_limit: usize) -> io::Result<Vec<u8>> {
    windows::read_only_powershell(script, output_limit)
}

#[cfg(test)]
mod tests {
    use super::parse_node_args;
    use std::{ffi::OsString, path::PathBuf};
    #[test]
    fn parses_unicode_and_spaced_absolute_path_without_splitting() {
        let args = [
            "vibeface-core",
            "serve",
            "--data-dir",
            "/Users/test/Node 数据 2",
        ]
        .map(OsString::from);
        assert_eq!(
            parse_node_args(&args).unwrap().to_string_lossy(),
            "/Users/test/Node 数据 2"
        );
    }
    #[test]
    fn rejects_relative_data_directory_and_unbounded_args() {
        assert!(parse_node_args(&["core".into(), "--data-dir=relative".into()]).is_none());
        assert!(parse_node_args(&vec![OsString::from("x"); 257]).is_none());
    }

    #[test]
    fn parser_never_splits_spaces_inside_absolute_paths() {
        let args = [
            "vibeface-core",
            "serve",
            "--data-dir",
            "/home/test/Node With Space",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            parse_node_args(&args).unwrap(),
            PathBuf::from("/home/test/Node With Space")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kern_procargs2_parser_keeps_argv_path_and_discards_environment_tail() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut raw = Vec::new();
        raw.extend_from_slice(&4_i32.to_ne_bytes());
        raw.extend_from_slice(b"/opt/vibeface-core\0\0");
        for arg in [
            "vibeface-core",
            "serve",
            "--data-dir",
            "/Users/user/Node 数据",
        ] {
            raw.extend_from_slice(arg.as_bytes());
            raw.push(0);
        }
        raw.extend_from_slice(b"API_KEY=never-return-this\0");
        let args = super::parse_kern_proc_args(&raw).unwrap();
        assert_eq!(
            args.last().unwrap(),
            &std::ffi::OsString::from_vec("/Users/user/Node 数据".as_bytes().to_vec())
        );
        assert_eq!(
            super::parse_node_args(&args).unwrap(),
            PathBuf::from("/Users/user/Node 数据")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("API_KEY"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_line_parser_keeps_quoted_unicode_path() {
        let args = super::windows::command_line_args("\"C:\\Program Files\\Vibeface\\vibeface-core.exe\" serve --data-dir \"C:\\Users\\User\\Node 数据\"").unwrap();
        assert_eq!(
            super::parse_node_args(&args).unwrap(),
            PathBuf::from("C:\\Users\\User\\Node 数据")
        );
    }
}
