//! Standalone fixture builds must receive Cargo's configured MSVC linker explicitly.
use std::{ffi::OsString, process::Command};

pub fn command() -> Command {
    #[cfg(windows)]
    let linker = std::env::var_os("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER");
    #[cfg(not(windows))]
    let linker = None;
    command_with_linker(linker)
}

pub(crate) fn command_with_linker(linker: Option<OsString>) -> Command {
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    if let Some(linker) = linker {
        let mut option = OsString::from("linker=");
        option.push(linker);
        command.arg("-C").arg(option);
    }
    command
}
