//! Finding and starting a program the same way on both families.
//!
//! Unix marks a program with an execute bit and starts it by path. Windows has no execute bit,
//! appends the extensions in `PATHEXT` to a bare name, and installs many command line tools as a
//! `.cmd` shim rather than an executable. Resolving all of that here keeps provider discovery,
//! the executable checks in front of every launch, and the launches themselves agreeing on one
//! answer.

use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

/// The extensions this account's shell treats as a program, lowercase and with the leading dot.
#[cfg(windows)]
pub fn program_extensions() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|entry| entry.starts_with('.') && entry.len() > 1)
        .map(|entry| entry.to_ascii_lowercase())
        .collect()
}

/// Whether `path` ends in an extension this account's shell runs.
#[cfg(windows)]
pub fn has_program_extension(path: &Path) -> bool {
    let Some(extension) = path.extension().map(|extension| {
        let mut dotted = String::from(".");
        dotted.push_str(&extension.to_string_lossy());
        dotted.to_ascii_lowercase()
    }) else {
        return false;
    };
    program_extensions().contains(&extension)
}

/// The file `name` resolves to on this account's `PATH`, or `None` when nothing does.
///
/// A name that already carries a directory separator is checked as given rather than searched.
pub fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    let direct = Path::new(name);
    if direct.components().count() > 1 {
        return candidates(direct).into_iter().find(|path| runnable(path));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .filter(|directory| !directory.as_os_str().is_empty())
            .flat_map(|directory| candidates(&directory.join(direct)))
            .find(|path| runnable(path))
    })
}

/// Every file name that could be the program `base` names.
pub fn candidates(base: &Path) -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        vec![base.to_owned()]
    }
    #[cfg(windows)]
    {
        if has_program_extension(base) {
            return vec![base.to_owned()];
        }
        let mut found = vec![base.to_owned()];
        let name = base.file_name().unwrap_or_default().to_string_lossy();
        for extension in program_extensions() {
            found.push(base.with_file_name(format!("{name}{extension}")));
        }
        found
    }
}

/// Whether `path` names a program this account can start.
pub fn runnable(path: &Path) -> bool {
    super::privatefs::is_executable(path)
}

/// Refuses `path` unless it names a program this account can run.
pub fn ensure_runnable(path: &Path, label: &str) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "{label} must be an absolute path: {}",
        path.display()
    );
    let metadata = std::fs::symlink_metadata(path).with_context(|| format!("read {label}"))?;
    ensure!(
        metadata.is_file() || metadata.file_type().is_symlink(),
        "{label} is not a program: {}",
        path.display()
    );
    ensure!(
        runnable(path),
        "{label} is not a program this account can run: {}",
        path.display()
    );
    Ok(())
}

/// A command that starts the program at `path`.
///
/// On Windows a `.cmd` or `.bat` entry point is not something the loader can start, so the
/// standard library routes it through the command interpreter and quotes its arguments for it.
/// Going through this function keeps every launch on that one path.
pub fn command(path: &Path) -> Command {
    Command::new(path)
}

/// Replaces this process with the program at `path`.
///
/// Unix hands the process over, so it keeps its process id and its place in the shell's job
/// control. Windows has no such call: the program runs as a child that inherits the standard
/// streams and this process ends with the status the child reported, which is what the caller
/// sees either way.
pub fn become_program<I, S>(path: &Path, args: I) -> anyhow::Error
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = Command::new(path).args(args).exec();
        anyhow::Error::new(error)
    }
    #[cfg(windows)]
    {
        match Command::new(path).args(args).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => anyhow::Error::new(error),
        }
    }
}
