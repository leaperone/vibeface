//! Files and directories only the account running this process can read or write, and writes
//! that leave either the old or the new content behind after a power loss.
//!
//! Unix enforces this with the owning uid and `0o600`/`0o700` modes. Windows enforces it with a
//! protected access control list whose only entry names the token user. Neither family has a
//! permissive fallback: a location that cannot carry the guarantee, such as a volume without
//! access control, is refused rather than accepted.
//!
//! Every open here refuses a link at the final component, so a link planted in place of a
//! private file cannot redirect a read or a write to somewhere this account does not control.

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{File, Metadata},
    io::{self, Write},
    path::Path,
};

/// An account, compared only against other accounts read the same way.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Account(AccountId);

#[cfg(unix)]
type AccountId = u32;
#[cfg(windows)]
type AccountId = super::winsec::Sid;

/// The account this process runs as.
pub fn current_account() -> Result<Account> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid reads the calling process and touches no memory.
        Ok(Account(unsafe { libc::geteuid() }))
    }
    #[cfg(windows)]
    {
        Ok(Account(super::winsec::current_user()?))
    }
}

/// The account that owns `path`.
pub fn owner_of(path: &Path) -> Result<Account> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(Account(std::fs::metadata(path)?.uid()))
    }
    #[cfg(windows)]
    {
        let _ = std::fs::metadata(path)?;
        super::winsec::owner_of_path(path)
            .map(Account)
            .map_err(Into::into)
    }
}

/// Whether `left` and `right` belong to the same account.
pub fn same_owner(left: &Path, right: &Path) -> Result<bool> {
    Ok(owner_of(left)? == owner_of(right)?)
}

pub fn owned_by_current_account(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    let owned = owner_of(path)? == current_account()?;
    #[cfg(windows)]
    let owned = super::winsec::path_owned_by_current_account(path)?;
    Ok(owned)
}

/// Whether writes and replacement of this installation path are limited to the current
/// account and system administrators.
pub fn is_trusted_install_path(path: &Path, ancestor: bool) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path)?;
        let owner = metadata.uid();
        let file_type = metadata.file_type();
        let mode = metadata.permissions().mode();
        let trusted_owner = owner == 0 || Account(owner) == current_account()?;
        let sticky_ancestor = ancestor && metadata.is_dir() && owner == 0 && mode & 0o1000 != 0;
        if !trusted_owner || !(file_type.is_symlink() || mode & 0o022 == 0 || sticky_ancestor) {
            return Ok(false);
        }
        #[cfg(target_os = "macos")]
        let safe = macos_install_acl_is_safe(path)?;
        #[cfg(not(target_os = "macos"))]
        let safe = true;
        Ok(safe)
    }
    #[cfg(windows)]
    {
        super::winsec::path_is_trusted_install(path, ancestor).map_err(Into::into)
    }
}

#[cfg(target_os = "macos")]
fn macos_install_acl_is_safe(path: &Path) -> io::Result<bool> {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    // Apple's <sys/acl.h> defines these types and values for extended ACLs.
    const EXTENDED: c_int = 0x100;
    const FIRST: c_int = 0;
    const NEXT: c_int = -1;
    const DENY: c_int = 2;
    unsafe extern "C" {
        fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, which: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }
    struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns a non-null ACL returned from acl_get_link_np.
            unsafe { acl_free(self.0) };
        }
    }

    let original_path = path;
    let before = std::fs::symlink_metadata(original_path)?;
    let path = CString::new(original_path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: the path is NUL terminated. This lookup does not follow the final symlink.
    let acl = unsafe { acl_get_link_np(path.as_ptr(), EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error
            .raw_os_error()
            .is_some_and(|code| [libc::ENOENT, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&code))
        {
            let after = std::fs::symlink_metadata(original_path)?;
            Ok((after.dev(), after.ino()) == (before.dev(), before.ino()))
        } else {
            Err(error)
        };
    }
    let acl = Acl(acl);
    let mut which = FIRST;
    loop {
        let mut entry = std::ptr::null_mut();
        // SAFETY: the ACL owns all returned entries and the out pointer is writable.
        if unsafe { acl_get_entry(acl.0, which, &mut entry) } != 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        let mut tag = 0;
        // SAFETY: the previous call returned an entry inside the live ACL.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if tag != DENY {
            return Ok(false);
        }
        which = NEXT;
    }
}

/// Whether no account other than this one can read or write `path`, which may be a file or a
/// directory.
pub fn is_private(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path)?;
        // SAFETY: geteuid reads the calling process and touches no memory.
        Ok(metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        let _ = std::fs::symlink_metadata(path)?;
        super::winsec::path_is_private(path).map_err(Into::into)
    }
}

/// Refuses `path` unless only this account can read or write it. `label` names the path in the
/// message a person reads.
pub fn ensure_private(path: &Path, label: &str) -> Result<()> {
    ensure!(
        is_private(path).with_context(|| format!("read the protection of {label}"))?,
        "{label} must stay reachable only by the account running this Node"
    );
    Ok(())
}

/// Whether `metadata` describes a regular file, so a directory, device, or link is refused
/// before it is read.
pub fn is_plain_file(metadata: &Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink()
}

/// Creates `path` and every missing parent, then restricts `path` itself to this account. An
/// existing directory keeps its contents and only has its protection reapplied.
pub fn create_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{} is not a directory",
            path.display()
        );
        return restrict_dir(path);
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        restrict_dir(path)
    }
    #[cfg(windows)]
    {
        match super::winsec::create_private_dir(path) {
            Ok(()) => Ok(()),
            // Another task created it first; reapplying the list keeps the guarantee.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => restrict_dir(path),
            Err(error) => Err(error.into()),
        }
    }
}

/// Creates `path` as a directory of this account alone, refusing a path that already exists.
///
/// A caller that must know it is writing somewhere fresh, such as a backup, uses this instead
/// of `create_dir`, which accepts a directory that is already there.
pub fn create_dir_new(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::create_dir(path)?;
        restrict_dir(path).map_err(io::Error::other)
    }
    #[cfg(windows)]
    {
        super::winsec::create_private_dir(path)
    }
}

/// Restricts an existing directory to this account, dropping anything inherited from a
/// permissive parent.
pub fn restrict_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        super::winsec::restrict_path(path, true).map_err(Into::into)
    }
}

/// Restricts an existing file to this account.
pub fn restrict_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        super::winsec::restrict_path(path, false).map_err(Into::into)
    }
}

/// Creates a private file, failing when one already exists at `path`.
pub fn create_new(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(windows)]
    {
        super::winsec::create_private_file(path, super::winsec::Disposition::New)
    }
}

/// Opens a private file for reading and writing, creating it when absent and keeping the
/// content of one that exists. The exclusive data lock is taken on a file opened this way.
pub fn open_or_create(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(windows)]
    {
        super::winsec::create_private_file(path, super::winsec::Disposition::Keep)
    }
}

/// What the filesystem calls this very file, which a copy or a restore from a backup does not
/// carry over. Unix uses the inode number, Windows the volume and file identifier.
pub fn file_identity(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(std::fs::metadata(path)?.ino().to_string())
    }
    #[cfg(windows)]
    {
        super::winsec::file_identity(path)
    }
}

/// Opens a private file that every write extends, creating it when absent. The log does this so
/// a rotated file keeps what it already holds.
pub fn append_or_create(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(windows)]
    {
        // Give the file its owner-only access control list when it is absent, then reopen it in
        // append mode so each write lands at the end even while another writer holds it.
        drop(super::winsec::create_private_file(
            path,
            super::winsec::Disposition::Keep,
        )?);
        std::fs::OpenOptions::new().append(true).open(path)
    }
}

/// Opens an existing file for reading without following a link at the final component, so a
/// link planted in place of a private file cannot redirect the read, and without waiting on
/// something that is not a file, so a pipe left in its place cannot hold the Node up.
pub fn open_no_follow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    }
    #[cfg(windows)]
    {
        super::winsec::open_no_reparse(path)
    }
}

/// Flushes the directory entry a create, rename, or unlink in `dir` produced.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
        Ok(())
    }
    #[cfg(windows)]
    {
        super::winsec::flush_directory(dir).map_err(Into::into)
    }
}

/// Moves `from` onto `to`, replacing any file already there, and does not return until the move
/// itself is on the disk.
pub fn durable_rename(from: &Path, to: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(from, to)?;
        if let Some(parent) = to.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        super::winsec::durable_move(from, to).map_err(Into::into)
    }
}

/// Creates `path` with `bytes`, refusing to replace a file that already exists, and leaves both
/// the content and the new directory entry on the disk.
pub fn create_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_new(path).context("create new private file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Replaces `path` with `bytes` through a private temporary file, so a reader sees either the
/// previous content or all of the new content and never a partial write.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .context("private file path has no name")?
        .to_string_lossy()
        .into_owned();
    let parent = path.parent().context("private file path has no parent")?;
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid()));
    let result = (|| -> Result<()> {
        let mut file = create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        durable_rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// A name no concurrent writer in this directory reuses.
fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Reads at most `limit` bytes of a private regular file, refusing a link, a directory, or a
/// file another account can reach.
pub fn read_private_bounded(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let metadata = std::fs::symlink_metadata(path).with_context(|| format!("read {label}"))?;
    ensure!(is_plain_file(&metadata), "{label} must be a regular file");
    ensure!(metadata.len() <= limit, "{label} exceeds its size limit");
    ensure_private(path, label)?;
    let file = open_no_follow(path).with_context(|| format!("open {label}"))?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{label} exceeds its size limit"
    );
    Ok(bytes)
}

/// Whether `path` names a program this account may run. Unix asks for an execute bit; Windows
/// has none and asks for an extension the shell runs.
pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        super::program::has_program_extension(path)
    }
}

/// Points `alias` at `target`, a directory, so the path a person or a program already holds
/// keeps working after an update moves the content.
///
/// Unix uses a symbolic link. Windows uses a directory junction, which an account without the
/// developer mode or administrator rights a symbolic link needs may still create.
pub fn link_dir(target: &Path, alias: &Path) -> Result<()> {
    ensure!(
        target.is_absolute(),
        "a managed directory link needs an absolute target"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, alias)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        super::winsec::create_junction(target, alias).map_err(Into::into)
    }
}

/// Reproduces the link at `source` as `target`, without following it.
///
/// A copy of a directory of this account has to carry its links across without reading through
/// them, so a link left behind by something else cannot pull the copy outside the directory.
pub fn copy_link(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(std::fs::read_link(source)?, target)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let pointed_at = read_dir_link(source).with_context(|| {
            format!(
                "copy the link at {}, which is not a directory link this account can reproduce",
                source.display()
            )
        })?;
        link_dir(&pointed_at, target)
    }
}

/// Whether `alias` is a managed directory link rather than a real directory.
pub fn is_dir_link(alias: &Path) -> Result<bool> {
    Ok(std::fs::symlink_metadata(alias)?.file_type().is_symlink())
}

/// Points the managed activation link `alias` at `target`, replacing whatever it pointed at.
///
/// Neither family leaves a moment where the link is missing. Unix writes a new link beside it
/// and renames it into place. Windows replaces the reparse record of the existing link in one
/// call, and a path is resolved when it is opened, so a reader always sees one target or the
/// other. Neither needs a privilege a normal account lacks.
pub fn point_dir_link(target: &Path, alias: &Path) -> Result<()> {
    ensure!(
        target.is_absolute(),
        "a managed directory link needs an absolute target"
    );
    let parent = alias
        .parent()
        .context("a managed directory link has no parent directory")?;
    #[cfg(unix)]
    {
        let temporary = parent.join(format!(".{}-{}", name_of(alias)?, uuid()));
        std::os::unix::fs::symlink(target, &temporary)?;
        if let Err(error) = std::fs::rename(&temporary, alias) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    #[cfg(windows)]
    match is_dir_link(alias) {
        Ok(true) => super::winsec::point_junction(target, alias)?,
        Ok(false) => bail!("{} is not a managed directory link", alias.display()),
        Err(_) => link_dir(target, alias)?,
    }
    sync_dir(parent)
}

/// The final component of `path`, as a name a temporary file can be built from.
#[cfg(unix)]
fn name_of(path: &Path) -> Result<std::borrow::Cow<'_, str>> {
    Ok(path
        .file_name()
        .context("a managed path has no name")?
        .to_string_lossy())
}

/// The directory `alias` points at, or an error when `alias` is not a link this module made.
pub fn read_dir_link(alias: &Path) -> Result<std::path::PathBuf> {
    let metadata = std::fs::symlink_metadata(alias)?;
    if !metadata.file_type().is_symlink() {
        bail!("{} is not a managed directory link", alias.display());
    }
    #[cfg(unix)]
    {
        Ok(std::fs::read_link(alias)?)
    }
    #[cfg(windows)]
    {
        super::winsec::read_junction(alias).map_err(Into::into)
    }
}

/// Removes a directory link without touching the directory it points at.
pub fn remove_dir_link(alias: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(alias)?;
    ensure!(
        metadata.file_type().is_symlink(),
        "{} is not a managed directory link",
        alias.display()
    );
    #[cfg(unix)]
    {
        std::fs::remove_file(alias)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        // A junction is a directory entry, so it is removed as one rather than unlinked.
        std::fs::remove_dir(alias)?;
        Ok(())
    }
}
