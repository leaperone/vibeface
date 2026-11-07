use super::{Host, LocalSourceError};
use anyhow::Result;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;
use vibeface_bridge::platform::tree;

const OUTPUT_LIMIT: usize = 320 * 1024;
const READ_LIMIT: Duration = Duration::from_secs(5);
/// Names a login shell's process tree in this account's job object namespace.
const SHELL_TREE_KEY: &str = "login-shell";

pub(super) struct Shell {
    pub path: PathBuf,
    pub user: String,
}

#[cfg(unix)]
fn registered_login_shell() -> Result<Option<Shell>> {
    let shell = vibeface_bridge::platform::account::login_shell().map_err(|_| LocalSourceError)?;
    Ok(shell.map(|shell| Shell {
        path: shell.path,
        user: shell.user,
    }))
}

pub(super) async fn missing(names: &[String], host: &Host) -> Result<BTreeMap<String, String>> {
    let mut inherited = BTreeMap::new();
    #[cfg(test)]
    inherited.extend(host.inherited_env.clone());
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
    let absent = || names.iter().any(|name| !inherited.contains_key(name));
    // Windows keeps this account's variables in the registry, where the value a person set
    // after the Node started is readable without running anything; a Unix login shell only
    // reveals one by running its startup scripts.
    #[cfg(windows)]
    if !cfg!(test) && absent() {
        let stored = vibeface_bridge::platform::account::stored(names)
            .map_err(|_| anyhow::Error::from(LocalSourceError))?;
        for (name, value) in stored {
            inherited.entry(name).or_insert(value);
        }
    }
    #[cfg(unix)]
    let shell = if !cfg!(test) && absent() {
        registered_login_shell()?
    } else {
        None
    };
    #[cfg(windows)]
    let shell = None;
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
    tree: tree::Tree,
}

impl Drop for ShellChild {
    fn drop(&mut self) {
        // Startup scripts may leave helpers behind, including after a successful read.
        self.tree.kill();
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
    let mut command = std::process::Command::new(&shell.path);
    command
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
        .stderr(Stdio::null());
    let (tree, child) =
        tree::Tree::spawn(command, SHELL_TREE_KEY, true).map_err(|_| LocalSourceError)?;
    let mut process = ShellChild { child, tree };
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
    process.tree.kill();
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
