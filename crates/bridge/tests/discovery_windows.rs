//! Live Windows enumeration of a fixture `vibeface-core.exe` owned by this account.

#![cfg(windows)]

use std::{
    fs,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use vibeface_bridge::platform::{discovery, privatefs};

const READY: &str = "vibeface-core-discovery-fixture-ready";

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FixtureProcess {
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
}
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

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
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("invoke rustc for the fixture core");
    assert!(
        output.status.success(),
        "fixture core must compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

#[test]
fn running_core_data_dirs_finds_fixture_process_with_spaced_unicode_data_dir() {
    let root = Scratch(scratch());
    let data_dir = root.0.join("Node 数据 With Space");
    privatefs::create_dir(&data_dir).expect("fixture data directory");
    let expected = data_dir.canonicalize().expect("canonical data directory");

    let core = compile_fixture_core(&root.0);
    let child = Command::new(&core)
        .arg("--data-dir")
        .arg(&data_dir)
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the fixture core");
    let mut process = FixtureProcess {
        child,
        reader: None,
    };
    let stderr = process.child.stderr.take().expect("fixture core stderr");
    let (ready, receive) = mpsc::channel();
    process.reader = Some(thread::spawn(move || {
        let mut line = String::new();
        if BufReader::new(stderr).read_line(&mut line).is_ok() {
            let _ = ready.send(line);
        }
    }));
    let marker = receive
        .recv_timeout(Duration::from_secs(20))
        .expect("fixture core ready marker");
    assert_eq!(marker.trim(), READY);

    let found = discovery::running_core_data_dirs().expect("enumerate running cores");
    assert!(
        found
            .iter()
            .any(|path| path.canonicalize().ok().as_ref() == Some(&expected)),
        "expected canonical data directory {expected:?} in {found:?}"
    );
}
