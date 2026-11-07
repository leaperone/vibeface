//! Live Windows enumeration of a fixture `vibeface-core.exe` owned by this account.

#![cfg(windows)]

use std::{
    fs,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use vibeface_bridge::platform::{discovery, privatefs};

const READY: &str = "vibeface-core-discovery-fixture-ready";

fn scratch() -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("vibeface-discovery-enum-{}", uuid::Uuid::new_v4()));
    privatefs::create_dir(&directory).expect("enumeration scratch directory");
    directory
}

fn compile_fixture_core(root: &Path) -> PathBuf {
    let source = root.join("fixture-core.rs");
    fs::write(
        &source,
        format!(
            r#"fn main() {{
    let _ = std::env::args().collect::<Vec<_>>();
    eprintln!("{READY}");
    loop {{
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }}
}}
"#
        ),
    )
    .expect("write the fixture core source");
    let executable = root.join("vibeface-core.exe");
    let output = Command::new("rustc")
        .args([
            source.to_string_lossy().as_ref(),
            "-o",
            executable.to_string_lossy().as_ref(),
        ])
        .output()
        .expect("invoke rustc for the fixture core");
    assert!(
        output.status.success(),
        "fixture core must compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

fn wait_for_ready(reader: &mut BufReader<impl std::io::Read>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        if line.trim() == READY {
            return;
        }
    }
    panic!("timed out waiting for the fixture core ready marker");
}

#[test]
fn running_core_data_dirs_finds_fixture_process_with_spaced_unicode_data_dir() {
    let root = scratch();
    let data_dir = root.join("Node 数据 With Space");
    privatefs::create_dir(&data_dir).expect("fixture data directory");
    let expected = data_dir.canonicalize().expect("canonical data directory");

    let core = compile_fixture_core(&root);
    let mut child = Command::new(&core)
        .arg("--data-dir")
        .arg(&data_dir)
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the fixture core");
    let stderr = child.stderr.take().expect("fixture core stderr");
    let mut reader = BufReader::new(stderr);
    wait_for_ready(&mut reader);

    let found = discovery::running_core_data_dirs().expect("enumerate running cores");
    assert!(
        found.iter().any(|path| path == &expected),
        "expected canonical data directory {expected:?} in {found:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
    fs::remove_dir_all(&root).expect("remove enumeration scratch");
}
