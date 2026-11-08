//! The variables this account set outside the processes it has already started.
//!
//! The two families keep them in different places. Unix keeps them in a login shell's startup
//! scripts, so the only way to see one is to run that shell; this module says which shell and
//! leaves the running to the caller. Windows keeps the per-user set in the registry, which this
//! module reads directly, so nothing has to be run and no startup script is involved.

use anyhow::Result;

/// The login shell registered for this account, and the account's name.
#[cfg(unix)]
pub struct LoginShell {
    pub path: std::path::PathBuf,
    pub user: String,
}

/// The login shell this account is registered with, or `None` when it has none.
#[cfg(unix)]
pub fn login_shell() -> Result<Option<LoginShell>> {
    use anyhow::bail;
    use std::{ffi::CStr, os::unix::ffi::OsStrExt as _};
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: the zeroed record is only read back after getpwuid_r reports success.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: the record, the buffer, and the result pointer are live locals, and the buffer
    // length is the one the call is given.
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
        bail!("this account has no password database entry");
    }
    // SAFETY: the entry was filled in by the call above, so both strings are NUL-terminated.
    let path = unsafe { CStr::from_ptr(entry.pw_shell) }.to_bytes();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(LoginShell {
        path: std::ffi::OsStr::from_bytes(path).into(),
        user: {
            // SAFETY: as above.
            unsafe { CStr::from_ptr(entry.pw_name) }
                .to_string_lossy()
                .into_owned()
        },
    }))
}

/// The values this account stored for `names`, as the account's own environment holds them
/// right now rather than as this process inherited them.
///
/// A name with no stored value is left out. Only string values are read: a name stored as
/// anything else is left out rather than guessed at.
#[cfg(windows)]
pub fn stored(names: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    use anyhow::Context as _;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, HANDLE},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW,
            RegQueryValueExW,
        },
    };

    /// The longest value this reads back, which is the longest a variable may be anywhere in
    /// the Node.
    const LIMIT: usize = 4096;

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    struct OpenKey(HKEY);

    impl Drop for OpenKey {
        fn drop(&mut self) {
            // SAFETY: the key was opened by the call below and is closed only here.
            unsafe { RegCloseKey(self.0) };
        }
    }

    let path = wide("Environment");
    let mut key: HKEY = std::ptr::null_mut::<HANDLE>().cast();
    // SAFETY: the name is NUL-terminated and the key handle is a live local.
    let opened = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) };
    if opened != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(opened as i32))
            .context("read the variables of this account");
    }
    let key = OpenKey(key);
    let mut values = std::collections::BTreeMap::new();
    for name in names {
        let name_wide = wide(name);
        let mut kind = 0_u32;
        let mut buffer = vec![0_u16; LIMIT];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: the name is NUL-terminated, and the kind, buffer, and length are live locals
        // whose sizes agree.
        let read = unsafe {
            RegQueryValueExW(
                key.0,
                name_wide.as_ptr(),
                std::ptr::null(),
                &mut kind,
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if read != ERROR_SUCCESS || !matches!(kind, REG_SZ | REG_EXPAND_SZ) {
            continue;
        }
        let characters = (bytes as usize / 2).min(buffer.len());
        let text = std::ffi::OsString::from_wide(&buffer[..characters]);
        let text = text.to_string_lossy();
        let text = text.trim_end_matches('\0');
        if !text.is_empty() {
            values.insert(name.clone(), text.to_owned());
        }
    }
    Ok(values)
}
