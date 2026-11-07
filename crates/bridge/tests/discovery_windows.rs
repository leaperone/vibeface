//! Live Windows enumeration of a fixture `vibeface-core.exe` owned by this account.

#![cfg(windows)]

use std::{
    fs,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use std::os::windows::process::CommandExt;

use vibeface_bridge::platform::{discovery, privatefs};

const READY: &str = "vibeface-core-discovery-fixture-ready";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const HELPER_MARKER: &str = "VIBEFACE_DISCOVERY_CONSOLE_PROOF";
const HELPER_EXPECTED: &str = "VIBEFACE_DISCOVERY_CONSOLE_PROOF_EXPECTED_DIR";
const HELPER_QUERY_TIMEOUT: Duration = Duration::from_secs(60);

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
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = workspace.join(format!(
        "target/vibeface-discovery-enum-{}",
        uuid::Uuid::new_v4()
    ));
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
    let mut compiler = Command::new("rustc");
    if let Some(linker) = std::env::var_os("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER") {
        let mut option = std::ffi::OsString::from("linker=");
        option.push(linker);
        compiler.arg("-C").arg(option);
    }
    let output = compiler
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

fn start_fixture() -> (Scratch, PathBuf, FixtureProcess) {
    let root = Scratch(scratch());
    let data_dir = root.0.join("Node 数据 With Space");
    privatefs::create_dir(&data_dir).expect("fixture data directory");
    let expected = data_dir.canonicalize().expect("canonical data directory");

    let core = compile_fixture_core(&root.0);
    let child = Command::new(&core)
        .arg("--data-dir")
        .arg(&expected)
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
    (root, expected, process)
}

fn assert_enumeration_matches(expected: &Path) {
    let found = discovery::running_core_data_dirs().expect("enumerate running cores");
    let count = found.len();
    let matched = found
        .iter()
        .any(|path| path.canonicalize().ok().as_deref() == Some(expected));
    assert!(
        matched,
        "enumeration miss: found_count={count} matched={matched}"
    );
}

fn wait_helper(child: &mut Child) {
    let deadline = Instant::now() + HELPER_QUERY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "helper exit success={}", status.success());
                let _ = child.wait();
                return;
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("helper wait failed");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("helper query timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_no_console_helper(expected: &Path) {
    let exe = std::env::current_exe().expect("current test executable");
    let token = uuid::Uuid::new_v4().to_string();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("running_core_data_dirs_finds_fixture_process_with_spaced_unicode_data_dir")
        .arg("--nocapture")
        .env(HELPER_MARKER, token)
        .env(HELPER_EXPECTED, expected)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("spawn no-console helper");
    wait_helper(&mut child);
}

#[test]
fn running_core_data_dirs_finds_fixture_process_with_spaced_unicode_data_dir() {
    if std::env::var_os(HELPER_MARKER).is_some() {
        let expected = std::env::var_os(HELPER_EXPECTED)
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok())
            .expect("helper expected directory");
        assert_enumeration_matches(&expected);
        return;
    }
    let (_root, expected, _process) = start_fixture();

    assert_enumeration_matches(&expected);
    spawn_no_console_helper(&expected);
}
