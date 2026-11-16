use super::fixture_rustc::command_with_linker;
use std::ffi::OsString;

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
