use super::{Host, LocalSourceError};
use anyhow::Result;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

const OUTPUT_LIMIT: usize = 320 * 1024;
const READ_LIMIT: Duration = Duration::from_secs(5);

pub(super) struct Shell {
    pub path: PathBuf,
    pub user: String,
}

fn registered_login_shell() -> Result<Option<Shell>> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let mut buffer = vec![0u8; 16 * 1024];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return Err(LocalSourceError.into());
    }
    let path = unsafe { CStr::from_ptr(entry.pw_shell) }.to_bytes();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(Shell {
        path: std::ffi::OsStr::from_bytes(path).into(),
        user: unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned(),
    }))
}

pub(super) async fn missing(names: &[String], host: &Host) -> Result<BTreeMap<String, String>> {
    let mut inherited = BTreeMap::new();
    if !cfg!(test) {
        for name in names {
            if let Some(value) = std::env::var_os(name) {
                let value = value.into_string().map_err(|_| LocalSourceError)?;
                if !value.is_empty() {
                    inherited.insert(name.clone(), value);
                }
            }
        }
    }
    let shell = if !cfg!(test) && names.iter().any(|name| !inherited.contains_key(name)) {
        registered_login_shell()?
    } else {
        None
    };
    missing_from(names, host, &inherited, shell.as_ref(), READ_LIMIT).await
}

pub(super) async fn missing_from(
    names: &[String],
    host: &Host,
    inherited: &BTreeMap<String, String>,
    shell: Option<&Shell>,
    limit: Duration,
) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let mut absent = Vec::new();
    for name in names {
        crate::providers::validate_env_name(name).map_err(|_| LocalSourceError)?;
        match inherited.get(name).filter(|value| !value.is_empty()) {
            Some(value) => {
                if value.len() > 4096 || value.contains('\0') {
                    return Err(LocalSourceError.into());
                }
                values.insert(name.clone(), value.clone());
            }
            None => absent.push(name.clone()),
        }
    }
    if !absent.is_empty()
        && let Some(shell) = shell
    {
        values.extend(read_shell(&absent, host, shell, limit).await?);
    }
    Ok(values)
}

struct ShellChild {
    child: tokio::process::Child,
    group: i32,
}

impl Drop for ShellChild {
    fn drop(&mut self) {
        // Startup scripts may leave helpers behind, including after a successful read.
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}

async fn read_shell(
    names: &[String],
    host: &Host,
    shell: &Shell,
    limit: Duration,
) -> Result<BTreeMap<String, String>> {
    let home = host.home.as_ref().ok_or(LocalSourceError)?;
    if !shell.path.is_absolute()
        || !home.is_absolute()
        || !matches!(
            shell.path.file_name().and_then(|name| name.to_str()),
            Some("zsh" | "bash" | "sh")
        )
        || !super::executable_file(&shell.path)
    {
        return Err(LocalSourceError.into());
    }
    let marker = format!("vibeface-{}", uuid::Uuid::new_v4().simple());
    let mut script = format!("printf '\\0%s\\0' '{marker}';\n");
    for name in names {
        script.push_str(&format!("printf '%s\\0%s\\0' '{name}' \"${{{name}-}}\";\n"));
    }
    script.push_str(&format!("printf '%s\\0' '{marker}:end'"));
    let child = Command::new(&shell.path)
        .args(["-ilc", &script])
        .env_clear()
        .env("HOME", home)
        .env("SHELL", &shell.path)
        .env("USER", &shell.user)
        .env("LOGNAME", &shell.user)
        .env("TERM", "dumb")
        .env(
            "PATH",
            host.path
                .as_deref()
                .unwrap_or(std::ffi::OsStr::new("/usr/bin:/bin")),
        )
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| LocalSourceError)?;
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id > 1)
        .ok_or(LocalSourceError)?;
    let mut process = ShellChild { child, group };
    let stdout = process.child.stdout.take().ok_or(LocalSourceError)?;
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(limit, async {
        stdout
            .take((OUTPUT_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| LocalSourceError)?;
        if bytes.len() > OUTPUT_LIMIT
            || !process
                .child
                .wait()
                .await
                .map_err(|_| LocalSourceError)?
                .success()
        {
            return Err(LocalSourceError.into());
        }
        parse(&bytes, &marker, names)
    })
    .await
    .map_err(|_| anyhow::Error::from(LocalSourceError))
    .and_then(|result| result);
    unsafe {
        libc::kill(-process.group, libc::SIGKILL);
    }
    let _ = tokio::time::timeout(Duration::from_millis(250), process.child.wait()).await;
    result
}

fn parse(bytes: &[u8], marker: &str, names: &[String]) -> Result<BTreeMap<String, String>> {
    let start = format!("\0{marker}\0");
    let offset = bytes
        .windows(start.len())
        .position(|part| part == start.as_bytes())
        .ok_or(LocalSourceError)?
        + start.len();
    let mut fields = bytes[offset..].split(|byte| *byte == 0);
    let mut values = BTreeMap::new();
    for name in names {
        if fields.next() != Some(name.as_bytes()) {
            return Err(LocalSourceError.into());
        }
        let value = fields.next().ok_or(LocalSourceError)?;
        if value.len() > 4096 {
            return Err(LocalSourceError.into());
        }
        let value = std::str::from_utf8(value).map_err(|_| LocalSourceError)?;
        if !value.is_empty() {
            values.insert(name.clone(), value.to_owned());
        }
    }
    if fields.next() != Some(format!("{marker}:end").as_bytes()) {
        return Err(LocalSourceError.into());
    }
    Ok(values)
}
