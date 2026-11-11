//! Standalone fixture builds must receive Cargo's configured MSVC linker explicitly.
use std::{ffi::OsString, process::Command};

pub fn command() -> Command {
    #[cfg(windows)]
    let linker = std::env::var_os("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER");
    #[cfg(not(windows))]
    let linker = None;
    command_with_linker(linker)
}

fn command_with_linker(linker: Option<OsString>) -> Command {
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    if let Some(linker) = linker {
        let mut option = OsString::from("linker=");
        option.push(linker);
        command.arg("-C").arg(option);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_msvc_linker_with_spaces_is_one_unquoted_compiler_argument() {
        let command = command_with_linker(Some(OsString::from(
            r"C:\Program Files\Microsoft Visual Studio\VC\bin\link.exe",
        )));
        let arguments = command
            .get_args()
            .map(|arg| arg.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            vec![
                OsString::from("-C"),
                OsString::from(r"linker=C:\Program Files\Microsoft Visual Studio\VC\bin\link.exe"),
            ]
        );
        assert_eq!(command_with_linker(None).get_args().count(), 0);
    }
}
