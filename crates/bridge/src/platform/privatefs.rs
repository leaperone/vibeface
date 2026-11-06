//! Files and directories only the account running this process can read or write, and writes
//! that leave either the old or the new content behind after a power loss.
//!
//! Unix enforces this with the owning uid and `0o600`/`0o700` modes. Windows enforces it with a
//! protected access control list whose only entry names the token user. Neither family has a
//! permissive fallback: a location that cannot carry the guarantee, such as a volume without
//! access control, is refused rather than accepted.

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
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
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
            .open(path)
    }
    #[cfg(windows)]
    {
        super::winsec::create_private_file(path, super::winsec::Disposition::New)
    }
}

/// Creates or replaces a private file, emptying one that already exists.
pub fn create_or_truncate(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(windows)]
    {
        super::winsec::create_private_file(path, super::winsec::Disposition::Truncate)
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
            .open(path)
    }
    #[cfg(windows)]
    {
        super::winsec::create_private_file(path, super::winsec::Disposition::Keep)
    }
}

/// Opens an existing file for reading without following a link at the final component, so a
/// link planted in place of a private file cannot redirect the read.
pub fn open_no_follow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
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
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
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
    ensure!(bytes.len() as u64 <= limit, "{label} exceeds its size limit");
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
