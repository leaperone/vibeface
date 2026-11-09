//! What the platform layer promises on the machine the tests run on.
//!
//! Every check here goes through the real operating system: real access control, a real local
//! endpoint, and real processes. The same source runs on both families, so a guarantee that
//! holds on Unix is proven again against the Windows calls that stand in for it.

use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use vibeface_bridge::platform::{ipc, privatefs, program, secret, tree};

/// A scratch directory of this account that no other test shares.
fn scratch(name: &str) -> PathBuf {
    // Unix keeps the parent short because a local endpoint bound inside it has a length limit.
    #[cfg(unix)]
    let parent = PathBuf::from("/tmp");
    #[cfg(windows)]
    let parent = std::env::temp_dir();
    let directory = parent.join(format!("vibeface-platform-{name}-{}", uuid::Uuid::new_v4()));
    privatefs::create_dir(&directory).expect("a private scratch directory");
    directory
}

/// A command that keeps running until it is ended, for the lifecycle checks.
fn long_lived() -> Command {
    #[cfg(unix)]
    {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 120"]);
        command
    }
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.args([
            "/c",
            "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 120\"",
        ]);
        command
    }
}

/// Writes a program of this account at `directory` that prints each argument on its own line.
fn echoing_program(directory: &Path, name: &str) -> PathBuf {
    #[cfg(unix)]
    {
        let path = directory.join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nfor part in \"$@\"; do printf '%s\\n' \"$part\"; done\n",
        )
        .expect("write the fixture program");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("mark the fixture program runnable");
        path
    }
    #[cfg(windows)]
    {
        let path = directory.join(format!("{name}.cmd"));
        std::fs::write(
            directory.join("echo-arguments.py"),
            "import sys\nfor argument in sys.argv[1:]: print(argument)\n",
        )
        .expect("write the native argument parser fixture");
        std::fs::write(
            &path,
            "@echo off\r\npython.exe \"%~dp0echo-arguments.py\" %*\r\n",
        )
        .expect("write the batch entry point used by Windows provider wrappers");
        path
    }
}

fn eventually(description: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {description}");
}

#[test]
fn private_paths_belong_to_this_account_and_are_written_whole() -> Result<()> {
    let directory = scratch("private");
    privatefs::ensure_private(&directory, "the scratch directory")?;

    let inside = directory.join("名前 with spaces");
    #[cfg(windows)]
    let inside = inside.join("nested-".repeat(30));
    #[cfg(windows)]
    assert!(inside.to_string_lossy().encode_utf16().count() > 260);
    privatefs::create_dir(&inside)?;
    privatefs::ensure_private(&inside, "a nested private directory")?;

    let file = inside.join("secret.json");
    privatefs::write_atomic(&file, b"first")?;
    assert_eq!(std::fs::read(&file)?, b"first");
    privatefs::ensure_private(&file, "a private file")?;
    privatefs::write_atomic(&file, b"second")?;
    assert_eq!(
        std::fs::read(&file)?,
        b"second",
        "a replacement leaves no half-written file behind"
    );

    let durable = inside.join("durable.bin");
    privatefs::create_durable(&durable, b"durable")?;
    privatefs::durable_rename(&durable, &inside.join("renamed.bin"))?;
    assert!(!durable.exists() && inside.join("renamed.bin").is_file());

    let log = inside.join("node.log");
    for line in ["one\n", "two\n"] {
        use std::io::Write as _;
        let mut handle = privatefs::append_or_create(&log)?;
        handle.write_all(line.as_bytes())?;
    }
    assert_eq!(std::fs::read_to_string(&log)?, "one\ntwo\n");
    privatefs::ensure_private(&log, "the log")?;

    let identity = privatefs::file_identity(&log)?;
    assert_eq!(
        identity,
        privatefs::file_identity(&log)?,
        "a file keeps its identity"
    );
    assert_ne!(
        identity,
        privatefs::file_identity(&file)?,
        "two files never share an identity"
    );

    assert!(
        privatefs::create_dir_new(&inside).is_err(),
        "a directory that must be fresh refuses one that is already there"
    );
    std::fs::remove_dir_all(&directory)?;
    Ok(())
}

#[test]
fn a_private_path_refuses_a_link_left_in_its_place() -> Result<()> {
    let directory = scratch("links");
    let elsewhere = directory.join("elsewhere");
    privatefs::create_dir(&elsewhere)?;
    std::fs::write(elsewhere.join("borrowed"), b"not this account's data")?;

    // A link where a private directory belongs is refused rather than followed, on both
    // families: Unix plants a symbolic link, Windows a directory junction.
    let planted = directory.join("providers");
    privatefs::link_dir(&elsewhere, &planted)?;
    assert!(privatefs::is_dir_link(&planted)?);
    assert!(
        privatefs::create_dir(&planted).is_err(),
        "a private directory is never taken from a link"
    );
    assert_eq!(
        privatefs::read_dir_link(&planted)?.canonicalize()?,
        elsewhere.canonicalize()?
    );

    let copied = directory.join("copied");
    privatefs::copy_link(&planted, &copied)?;
    assert_eq!(
        privatefs::read_dir_link(&copied)?.canonicalize()?,
        elsewhere.canonicalize()?,
        "a copy carries the link across instead of reading through it"
    );

    privatefs::point_dir_link(&directory, &planted)?;
    assert_eq!(
        privatefs::read_dir_link(&planted)?.canonicalize()?,
        directory.canonicalize()?,
        "an activation link is repointed in place"
    );
    std::fs::remove_dir_all(&directory)?;
    Ok(())
}

#[tokio::test]
async fn a_local_endpoint_admits_this_account_and_refuses_a_second_holder() -> Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let directory = ipc::runtime_dir(&uuid::Uuid::new_v4().simple().to_string())?;
    privatefs::create_dir(&directory)?;
    let endpoint = ipc::Endpoint::in_private_dir(&directory, "maintenance.sock")?;
    endpoint.ensure_private("the endpoint")?;
    assert!(!endpoint.is_held());

    let mut listener = ipc::Listener::claim(&endpoint).await?;
    assert!(endpoint.is_held());
    assert!(
        ipc::Listener::bind(&endpoint).await.is_err(),
        "a second holder of one endpoint is refused"
    );

    let connecting = tokio::spawn({
        let endpoint = endpoint.clone();
        async move {
            let mut client = ipc::Stream::connect(&endpoint).await?;
            client.write_all(b"ping\n").await?;
            client.flush().await?;
            let mut answer = [0_u8; 5];
            client.read_exact(&mut answer).await?;
            anyhow::Ok(answer)
        }
    });
    let mut served = listener.accept().await?;
    served.require_current_account("the endpoint")?;
    let mut request = [0_u8; 5];
    served.read_exact(&mut request).await?;
    assert_eq!(&request, b"ping\n");
    served.write_all(b"pong\n").await?;
    served.flush().await?;
    assert_eq!(&connecting.await??, b"pong\n");

    assert!(
        ipc::Listener::claim(&endpoint).await.is_err(),
        "a live holder is never displaced"
    );

    drop(served);
    drop(listener);
    tokio::time::timeout(Duration::from_secs(20), async {
        while endpoint.is_held() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    let (mut left, mut right) = ipc::pair().await?;
    left.write_all(b"paired\n").await?;
    left.flush().await?;
    let mut read = [0_u8; 7];
    right.read_exact(&mut read).await?;
    assert_eq!(&read, b"paired\n");
    std::fs::remove_dir_all(&directory)?;
    Ok(())
}

#[tokio::test]
async fn a_process_tree_is_cleared_when_the_node_ends_it() -> Result<()> {
    let (mut spawned, mut child) = tree::Tree::spawn(long_lived(), "platform-test", true)?;
    let root = spawned.root();
    assert!(spawned.is_running());
    assert!(tree::root_is_running(root));
    assert!(tree::token_is_running(spawned.token()));

    let left = tree::stop_recorded(spawned.token(), |_| false)?;
    assert!(
        left.is_none(),
        "a tree the Node does not recognize is left running"
    );
    assert!(spawned.is_running());

    assert!(vibeface_bridge::agent::terminate_process_tree(&mut child, &mut spawned).await);
    eventually("the tree to be cleared", || !spawned.is_running());
    assert!(!tree::token_is_running(spawned.token()));

    #[cfg(unix)]
    for graceful in [true, false] {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
        let directory = scratch("group-grace");
        let release = directory.join("release");
        let completed = directory.join("completed");
        let lost_anchor = directory.join("lost-anchor");
        let mut root = Command::new("python3");
        root.args(["-c", "import time; time.sleep(120)"])
            .stdout(Stdio::piped());
        let (mut spawned, mut child) = tree::Tree::spawn(root, "platform-grace", true)?;
        let token = spawned.token().to_owned();
        let mut root_stdout = child.stdout.take().unwrap();
        let mut member = Command::new("python3");
        member
            .args([
                "-c",
                r#"
import os, pathlib, signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
print('ready', flush=True)
while True:
    try:
        os.kill(int(sys.argv[3]), 0)
    except ProcessLookupError:
        pathlib.Path(sys.argv[4]).write_text('reaped')
    if pathlib.Path(sys.argv[1]).exists():
        break
    time.sleep(0.01)
pathlib.Path(sys.argv[2]).write_text('completed')
"#,
            ])
            .arg(&release)
            .arg(&completed)
            .arg(spawned.root().to_string())
            .arg(&lost_anchor)
            .stdout(Stdio::piped())
            .process_group(spawned.root() as i32);
        let mut member = tokio::process::Command::from(member)
            .kill_on_drop(true)
            .spawn()?;
        let mut ready = String::new();
        BufReader::new(member.stdout.take().unwrap())
            .read_line(&mut ready)
            .await?;
        assert_eq!(ready, "ready\n");
        let started = Instant::now();
        let (cleared, status) = tokio::join!(
            biased;
            vibeface_bridge::agent::terminate_process_tree(&mut child, &mut spawned),
            async {
                root_stdout.read_to_end(&mut Vec::new()).await?;
                if graceful { std::fs::write(&release, b"released")?; }
                member.wait().await
            }
        );
        let status = status?;
        assert!(
            !lost_anchor.exists(),
            "the owned leader stays unreaped until group signalling is finished"
        );
        assert!(
            cleared && !tree::token_is_running(&token),
            "the whole group is reaped before completion"
        );
        if graceful {
            assert!(
                status.success() && completed.exists(),
                "cooperative group members can finish during the grace period"
            );
        } else {
            assert!(
                started.elapsed() >= Duration::from_secs(3),
                "a running member keeps the full grace period after its root exits"
            );
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            assert!(!completed.exists());
        }
        std::fs::remove_dir_all(directory)?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_reaped_root_does_not_authorize_more_group_signals() -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    let directory = scratch("reaped-group");
    let signalled = directory.join("signalled");
    let mut root = Command::new("python3");
    root.args(["-c", "import time; time.sleep(120)"]);
    let (mut spawned, mut child) = tree::Tree::spawn(root, "platform-reaped", true)?;
    let mut member = Command::new("python3");
    member
        .args([
            "-c",
            "import pathlib, signal, sys, time; signal.signal(signal.SIGTERM, lambda *_: pathlib.Path(sys.argv[1]).write_text('term')); print('ready', flush=True); time.sleep(120)",
        ])
        .arg(&signalled)
        .stdout(Stdio::piped())
        .process_group(spawned.root() as i32);
    let mut member = tokio::process::Command::from(member)
        .kill_on_drop(true)
        .spawn()?;
    let mut ready = String::new();
    BufReader::new(member.stdout.take().unwrap())
        .read_line(&mut ready)
        .await?;
    assert_eq!(ready, "ready\n");
    child.start_kill()?;
    child.wait().await?;
    assert!(child.id().is_none());

    let cleared = vibeface_bridge::agent::terminate_process_tree(&mut child, &mut spawned).await;
    let remained = member.try_wait()?.is_none();
    let received_term = signalled.exists();
    let _ = member.start_kill();
    member.wait().await?;
    std::fs::remove_dir_all(directory)?;
    assert!(!cleared, "an unowned group remains unconfirmed");
    assert!(
        remained && !received_term,
        "a reaped root cannot authorize TERM or KILL to its numeric group"
    );
    Ok(())
}

#[tokio::test]
async fn a_recorded_tree_is_ended_only_when_the_record_still_names_it() -> Result<()> {
    let (mut spawned, mut child) = tree::Tree::spawn(long_lived(), "platform-record", true)?;
    let token = spawned.token().to_owned();

    // Windows hands a process identifier to a new program as soon as the old one is gone, so a
    // record whose start moment no longer matches must end nothing at all.
    #[cfg(windows)]
    {
        let mut fields = token.splitn(3, '|');
        let process: u32 = fields.next().expect("a recorded process").parse()?;
        let started: u64 = fields.next().expect("a recorded start").parse()?;
        let key = fields.next().expect("a recorded key");
        let reused = format!("{process}|{}|{key}", started + 1);
        assert!(
            tree::stop_recorded(&reused, |_| true).is_err(),
            "a record whose identifier was reused is refused"
        );
        assert!(
            spawned.is_running(),
            "nothing is ended when a record no longer names its program"
        );
    }

    let members = tree::stop_recorded(&token, |member| {
        #[cfg(unix)]
        return member.contains("sleep");
        #[cfg(windows)]
        return tree::names_program(member, Path::new("cmd.exe"));
    })?;
    assert!(
        members.is_some_and(|members| !members.is_empty()),
        "the Node's own tree is ended and its programs reported"
    );
    spawned.kill();
    let _ = child.start_kill();
    child.wait().await?;
    Ok(())
}

#[test]
fn a_program_of_this_account_is_found_and_its_arguments_are_passed_whole() -> Result<()> {
    let directory = scratch("programs");
    let program = echoing_program(&directory, "echoing");
    assert!(privatefs::is_executable(&program));
    assert!(program::runnable(&program));
    program::ensure_runnable(&program, "the fixture program")?;
    assert!(
        program::ensure_runnable(&directory, "a directory").is_err(),
        "a directory is not a program"
    );
    assert!(
        program::ensure_runnable(Path::new("echoing"), "a bare name").is_err(),
        "a program is named by its full path"
    );

    let awkward = "two words --and-a-flag \"quoted\"";
    let output = program::command(&program)
        .arg(awkward)
        .arg("second")
        .output()?;
    assert!(output.status.success());
    let lines: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines,
        vec![awkward.to_owned(), "second".to_owned()],
        "an argument reaches the program as one word, however it is spelled"
    );

    let looked_for = program::candidates(&directory.join("echoing"));
    assert!(
        looked_for
            .iter()
            .any(|candidate| program::runnable(candidate)),
        "a program is looked for by the names this family gives one"
    );
    std::fs::remove_dir_all(&directory)?;
    Ok(())
}

#[test]
fn a_handed_over_channel_refuses_what_another_account_could_read() -> Result<()> {
    let directory = scratch("secret");
    let path = directory.join("passphrase");
    privatefs::write_atomic(&path, b"a synthetic test passphrase\n")?;
    let private = std::fs::File::open(&path)?;
    let mut handed = secret::handed_over(reference(&private))?;
    use std::io::Read as _;
    let mut read = String::new();
    handed.read_to_string(&mut read)?;
    assert_eq!(read, "a synthetic test passphrase\n");

    let shared = widely_readable(&path)?;
    let opened = std::fs::File::open(shared)?;
    assert!(
        secret::handed_over(reference(&opened)).is_err(),
        "a channel another account could read is refused"
    );
    std::fs::remove_dir_all(&directory)?;
    Ok(())
}

/// The value a caller hands over to name an open file: a descriptor on Unix, a handle value on
/// Windows.
fn reference(file: &std::fs::File) -> i64 {
    #[cfg(unix)]
    {
        std::os::fd::AsRawFd::as_raw_fd(file) as i64
    }
    #[cfg(windows)]
    {
        std::os::windows::io::AsRawHandle::as_raw_handle(file) as i64
    }
}

/// A file holding readable bytes that accounts other than this one can reach.
fn widely_readable(path: &Path) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
        Ok(path.to_owned())
    }
    #[cfg(windows)]
    {
        // Windows carries access in a list only its own calls can widen, so a file every
        // account on the machine may read is borrowed from the system instead.
        let _ = path;
        let root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let shared = PathBuf::from(root).join("System32\\drivers\\etc\\hosts");
        assert!(shared.is_file(), "a widely readable file is required");
        Ok(shared)
    }
}

#[cfg(windows)]
#[tokio::test]
async fn codex_uses_a_native_unix_socket_instead_of_an_invented_pipe_url() -> Result<()> {
    use std::io::{Read as _, Write as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let key = uuid::Uuid::new_v4().simple().to_string();
    let directory = ipc::runtime_dir(&key[..12])?;
    privatefs::create_dir(&directory)?;
    let endpoint = ipc::Endpoint::codex_in_private_dir(&directory)?;
    assert_eq!(
        endpoint.listen_argument()?,
        format!("unix://{}", endpoint.path().display())
    );
    let native = uds_windows::UnixListener::bind(endpoint.path())?;
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = native.accept()?;
        let mut request = [0; 4];
        stream.read_exact(&mut request)?;
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong")
    });
    let mut client = ipc::Stream::connect(&endpoint).await?;
    client.require_current_account("the native Codex socket")?;
    client.write_all(b"ping").await?;
    client.flush().await?;
    let mut response = [0; 4];
    client.read_exact(&mut response).await?;
    assert_eq!(&response, b"pong");
    drop(client);
    server.join().expect("native socket fixture thread")?;
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

#[tokio::test]
async fn two_programs_with_the_same_label_have_independent_trees() -> Result<()> {
    let (mut first, mut first_child) = tree::Tree::spawn(long_lived(), "same-provider", true)?;
    let (mut second, mut second_child) = tree::Tree::spawn(long_lived(), "same-provider", true)?;
    let distinct = first.token() != second.token();
    first.stop();
    let _ = first_child.wait().await;
    let survived = second.is_running();
    second.stop();
    let _ = second_child.wait().await;
    assert!(distinct, "each launch records its own process tree");
    assert!(
        survived,
        "ending one provider does not end another with the same label"
    );
    Ok(())
}

#[cfg(windows)]
#[test]
fn an_inherited_acl_is_not_a_protected_private_file() -> Result<()> {
    let directory = scratch("acl-inheritance");
    let file = directory.join("inherited");
    std::fs::write(&file, b"fixture")?;
    assert!(privatefs::ensure_private(&file, "the inherited fixture").is_err());
    privatefs::restrict_file(&file)?;
    privatefs::ensure_private(&file, "the protected fixture")?;
    std::fs::remove_dir_all(directory)?;
    Ok(())
}
