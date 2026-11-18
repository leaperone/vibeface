use super::fixture_rustc;
use crate::host;

use anyhow::{Context, Result, bail};
use host::{
    BackgroundAction, DesktopHost, NodeIdentity, NodeMode, NodeStatus, ServiceState,
    UpdateAvailability,
    activation::{Activation, ActivationQueue, InstanceClaim, SingleInstance},
    platform::{SaveResult, save_selected},
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use vibeface_bridge::platform::privatefs;

struct CompiledFixture {
    executable: PathBuf,
}
impl Drop for CompiledFixture {
    fn drop(&mut self) {
        if let Some(parent) = self.executable.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
struct Fixture {
    root: PathBuf,
    profile: PathBuf,
    runtime: PathBuf,
    _compiled: Arc<CompiledFixture>,
}
impl Fixture {
    fn new() -> Result<Self> {
        static EXECUTABLE: OnceLock<Mutex<Weak<CompiledFixture>>> = OnceLock::new();
        let mut cache = EXECUTABLE
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .unwrap();
        let compiled = cache.upgrade().unwrap_or_else(|| {
            let directory =
                std::env::temp_dir().join(format!("vf-host-fixture-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let executable = directory.join(if cfg!(windows) { "child.exe" } else { "child" });
            let source =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_host_child.rs");
            let status = fixture_rustc::command()
                .arg("--edition=2024")
                .arg(source)
                .arg("-o")
                .arg(&executable)
                .status()
                .expect("compile isolated synthetic child with the existing toolchain");
            assert!(status.success());
            let compiled = Arc::new(CompiledFixture { executable });
            *cache = Arc::downgrade(&compiled);
            compiled
        });
        drop(cache);
        let root = std::env::temp_dir().join(format!(
            "vfh-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        privatefs::create_dir(&root)?;
        let root = std::fs::canonicalize(root)?;
        let profile = root.join("profile");
        let runtime = root.join("runtime");
        privatefs::create_dir(&profile)?;
        privatefs::create_dir(&runtime)?;
        for name in ["vibeface-core", "vibeface"] {
            std::fs::copy(
                &compiled.executable,
                runtime.join(if cfg!(windows) {
                    format!("{name}.exe")
                } else {
                    name.into()
                }),
            )?;
        }
        Ok(Self {
            root,
            profile,
            runtime,
            _compiled: compiled,
        })
    }
    fn open(&self, attach_only: bool) -> Result<DesktopHost> {
        DesktopHost::open(self.profile.clone(), self.runtime.clone(), attach_only)
    }
    fn node(&self) -> PathBuf {
        self.profile.join("node")
    }
    fn initialize(&self) -> Result<()> {
        // The dependency-free synthetic child does not implement Windows DACL creation.
        // These lifecycle scenarios use a Node provisioned by the real private filesystem API.
        privatefs::create_dir(&self.node())?;
        privatefs::write_atomic(&self.node().join("config.json"), b"{}")?;
        privatefs::write_atomic(&self.node().join("identity.json"), b"{}")?;
        privatefs::ensure_private(&self.node(), "controlled Node directory")?;
        for name in ["config.json", "identity.json"] {
            let path = self.node().join(name);
            assert!(privatefs::is_plain_file(&std::fs::symlink_metadata(&path)?));
            privatefs::ensure_private(&path, "controlled Node file")?;
            assert_eq!(
                privatefs::read_private_bounded(&path, 1024, "controlled Node file")?,
                b"{}"
            );
        }
        Ok(())
    }
    fn identity(&self) -> NodeIdentity {
        NodeIdentity {
            data_dir: self.node(),
            device_id: "a".repeat(64),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
async fn wait_file(path: &Path, exists: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while path.exists() != exists {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("synthetic child did not reach the expected state")
}

async fn wait_parent_ready(parent: &mut tokio::process::Child, path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if path.try_exists()? {
                return Ok(());
            }
            if let Some(status) = parent.try_wait()? {
                let mut stderr = Vec::new();
                if let Some(stream) = parent.stderr.take() {
                    stream.take(64 * 1024).read_to_end(&mut stderr).await?;
                }
                bail!(
                    "synthetic GUI parent exited before readiness ({status}): {}",
                    String::from_utf8_lossy(&stderr)
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("synthetic GUI parent did not reach the expected state")?
}

#[tokio::test]
async fn client_only_discovery_settings_and_forget_never_initialize_node() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut host = fixture.open(true)?;
    assert!(host.discover().await?.nodes.is_empty());
    host.choose_mode(NodeMode::Controller)?;
    assert!(host.start_owned().await.is_err());
    assert!(host.background(BackgroundAction::Install).await.is_err());
    host.clear_selection()?;
    host.shutdown().await?;
    assert!(!fixture.node().exists());
    assert_eq!(
        fixture.open(true)?.snapshot().node_mode,
        NodeMode::Controller
    );
    // A missing bundle also leaves a fully usable client/profile and no Node directory.
    let host = DesktopHost::open(fixture.profile.clone(), fixture.root.join("missing"), true)?;
    assert!(host.snapshot().owned.is_none());
    assert!(!fixture.node().exists());
    Ok(())
}
#[test]
fn discovery_prefers_only_a_matching_running_identity_or_a_sole_running_node() {
    use host::{Discovery, LocalNode, NodeSource, preferred_running_node};

    let node = |name: &str, id: &str, status| LocalNode {
        data_dir: std::env::temp_dir().join(name),
        device_id: id.repeat(64),
        sources: vec![NodeSource::Remembered],
        status,
        unavailable_reason: None,
        local_listen: None,
        listen: None,
        web_enabled: None,
        core_version: None,
        user_service_cli: None,
    };
    let remembered = node("remembered", "a", NodeStatus::Running);
    let other = node("other", "b", NodeStatus::Running);
    let mut discovery = Discovery {
        nodes: vec![remembered.clone(), other.clone()],
        ..Default::default()
    };
    assert_eq!(preferred_running_node(&discovery, None), None);
    assert_eq!(
        preferred_running_node(&discovery, Some(&remembered.identity())),
        Some(remembered.identity())
    );
    discovery.nodes[0].device_id = "c".repeat(64);
    assert_eq!(
        preferred_running_node(&discovery, Some(&remembered.identity())),
        None
    );
    discovery.nodes[0].status = NodeStatus::Stopped;
    assert_eq!(
        preferred_running_node(&discovery, Some(&remembered.identity())),
        Some(other.identity())
    );
}

#[tokio::test]
async fn backend_startup_discovers_an_existing_node_without_a_host_command() -> Result<()> {
    use crate::views::backend::{Backend, Event};
    use host::DiscoveryState;

    let fixture = Fixture::new()?;
    fixture.initialize()?;
    privatefs::write_atomic(&fixture.node().join("running"), b"fixture")?;
    let (commands, mut events, _snapshots, stop, actor) = Backend::spawn(
        fixture.open(false)?,
        &tokio::runtime::Handle::current(),
        "fixture-startup".into(),
    );
    let selected = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Event::Scoped { event, .. }) = events.recv().await {
            if let Event::Host(host) = *event
                && host.discovery_state == DiscoveryState::Ready
            {
                assert!(host.owned.is_none());
                assert!(!host.can_start_owned);
                return host.selected;
            }
        }
        None
    })
    .await?;
    assert_eq!(selected, Some(fixture.identity()));
    assert!(!fixture.profile.join("fixture.calls").exists());
    assert!(
        !fixture
            .profile
            .join("native-desktop-settings.json")
            .exists()
    );
    stop.send(true)?;
    drop(commands);
    tokio::time::timeout(Duration::from_secs(5), actor).await??;
    assert!(fixture.node().join("running").exists());
    Ok(())
}

#[tokio::test]
async fn owned_start_stop_and_drop_close_only_the_held_child_stdin() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    let mut host = fixture.open(false)?;
    let running = host.start_owned().await?;
    assert_eq!(running.status, NodeStatus::Running);
    assert!(host.snapshot().owned.is_some());
    host.stop_owned().await?;
    assert_eq!(
        std::fs::read(fixture.node().join("fixture.exited"))?,
        b"eof"
    );
    assert!(!fixture.node().join("running").exists());
    host.start_owned().await?;
    drop(host);
    wait_file(&fixture.node().join("running"), false).await?;
    assert!(fixture.node().join("identity.json").exists());
    Ok(())
}
#[tokio::test]
async fn attached_process_survives_host_quit_and_directory_forgetting() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    let binary = fixture.runtime.join(if cfg!(windows) {
        "vibeface-core.exe"
    } else {
        "vibeface-core"
    });
    let mut process = tokio::process::Command::new(binary)
        .arg("--data-dir")
        .arg(fixture.node())
        .arg("--desktop-parent-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = process.stdin.take();
    wait_file(&fixture.node().join("running"), true).await?;
    let mut host = fixture.open(true)?;
    host.discover().await?;
    host.select_node(&fixture.identity()).await?;
    host.clear_selection()?;
    host.shutdown().await?;
    drop(host);
    assert!(
        process.try_wait()?.is_none(),
        "an attached child remains owned by its actual parent"
    );
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(3), process.wait()).await??;
    assert!(fixture.node().join("identity.json").exists());
    Ok(())
}
#[tokio::test]
async fn changed_identity_and_untrusted_paths_are_rejected_before_start() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    let mut host = fixture.open(false)?;
    assert!(
        host.remember_directory(PathBuf::from("relative"))
            .await
            .is_err()
    );
    assert!(
        host.inspect(&fixture.profile.join("../profile/node"))
            .await
            .is_err()
    );
    privatefs::write_atomic(
        &fixture.node().join("fixture.identity"),
        "b".repeat(64).as_bytes(),
    )?;
    assert!(host.select_node(&fixture.identity()).await.is_err());
    assert!(!fixture.node().join("running").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::set_permissions(
            fixture.node().join("identity.json"),
            std::fs::Permissions::from_mode(0o644),
        )?;
        assert!(host.inspect(&fixture.node()).await.is_err());
        std::fs::remove_file(fixture.node().join("identity.json"))?;
        symlink(
            fixture.node().join("config.json"),
            fixture.node().join("identity.json"),
        )?;
        assert!(host.inspect(&fixture.node()).await.is_err());
    }
    Ok(())
}
#[tokio::test]
async fn service_and_runtime_updates_remain_distinct_and_require_fresh_state() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    let mut host = fixture.open(false)?;
    let status = host.background(BackgroundAction::Install).await?;
    assert_eq!(status.state, ServiceState::Installed);
    assert!(host.snapshot().owned.is_none());
    assert!(host.start_owned().await.is_err());
    privatefs::write_atomic(&fixture.profile.join("fixture.recovery"), b"")?;
    assert!(
        host.background(BackgroundAction::Update).await.is_err(),
        "pending recovery must stop the service first"
    );
    std::fs::remove_file(fixture.profile.join("fixture.recovery"))?;
    assert!(
        host.background(BackgroundAction::Update).await.is_err(),
        "current runtime does not update"
    );
    privatefs::write_atomic(
        &fixture.profile.join("fixture.availability"),
        b"immutable_conflict",
    )?;
    assert!(host.background(BackgroundAction::Update).await.is_err());
    privatefs::write_atomic(
        &fixture.profile.join("fixture.availability"),
        b"downgrade_refused",
    )?;
    assert!(host.background(BackgroundAction::Update).await.is_err());
    assert_eq!(
        std::fs::read_to_string(fixture.profile.join("fixture.last-action"))?,
        "install"
    );
    privatefs::write_atomic(
        &fixture.profile.join("fixture.availability"),
        b"newer_available",
    )?;
    let updated = host.background(BackgroundAction::Update).await?;
    assert_eq!(
        updated.runtime_update.availability,
        UpdateAvailability::Current
    );
    host.shutdown().await?;
    assert!(fixture.node().join("running").exists());
    host.background(BackgroundAction::Stop).await?;
    let started = host.background(BackgroundAction::Start).await?;
    assert_eq!(started.state, ServiceState::Installed);
    assert_eq!(started.service_running, Some(true));
    assert_eq!(started.core_status, Some(NodeStatus::Running));
    host.background(BackgroundAction::Stop).await?;
    privatefs::write_atomic(&fixture.profile.join("fixture.recovery"), b"")?;
    let partial = serde_json::json!({
        "state":"partial", "device_id":"a".repeat(64), "data_dir":fixture.node(),
        "service_id":"fixture.service", "managed_root":fixture.profile.join("managed"),
        "service_running":false, "core_status":"stopped",
        "runtime_update":{"bundled_version":"2.0.0","installed_version":"2.0.0",
            "running_version":null,"availability":"current","recovery_required":false}
    });
    privatefs::write_atomic(
        &fixture.profile.join("fixture.mutation-status"),
        &serde_json::to_vec(&partial)?,
    )?;
    let recovered = host.background(BackgroundAction::Update).await?;
    assert_eq!(recovered.state, ServiceState::Partial);
    assert!(!recovered.runtime_update.recovery_required);
    assert_eq!(
        host.snapshot().background.unwrap().state,
        ServiceState::Partial
    );
    std::fs::remove_file(fixture.profile.join("fixture.mutation-status"))?;
    let removed = host.background(BackgroundAction::Uninstall).await?;
    assert_eq!(removed.state, ServiceState::NotInstalled);
    assert!(fixture.node().join("identity.json").exists());
    Ok(())
}
#[tokio::test]
async fn failed_mutations_are_reconciled_by_status_without_repeating_the_write() -> Result<()> {
    for fault in ["fixture.bad-mutation", "fixture.nonzero-mutation"] {
        let fixture = Fixture::new()?;
        fixture.initialize()?;
        let mut host = fixture.open(false)?;
        host.background(BackgroundAction::Install).await?;
        privatefs::write_atomic(
            &fixture.profile.join("fixture.availability"),
            b"newer_available",
        )?;
        privatefs::write_atomic(&fixture.profile.join(fault), b"")?;
        assert!(host.background(BackgroundAction::Update).await.is_err());
        let calls = std::fs::read_to_string(fixture.profile.join("fixture.calls"))?;
        let updates: Vec<_> = calls
            .lines()
            .filter(|line| line.ends_with("\tupdate"))
            .collect();
        assert_eq!(updates.len(), 1, "unknown mutation is never retried");
        assert!(updates[0].contains(&format!("--expected-device-id\t{}\tupdate", "a".repeat(64))));
        assert!(calls.lines().last().unwrap().ends_with("\tstatus"));
        let reconciled = host.snapshot().background.unwrap();
        assert_eq!(
            reconciled.runtime_update.availability,
            UpdateAvailability::Current
        );
        assert_eq!(reconciled.identity(), Some(fixture.identity()));
    }
    Ok(())
}

// /proc disappearance proves this particular child was reaped as well as killed.
// Other platforms retain the malformed/nonzero cases but do not run this Linux proof.
#[cfg(target_os = "linux")]
#[tokio::test(start_paused = true)]
async fn timed_out_update_after_write_is_reaped_and_reconciled_without_retry() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    for (name, value) in [
        ("service.installed", b"".as_slice()),
        ("fixture.availability", b"newer_available".as_slice()),
        ("fixture.timeout-after-mutation", b"".as_slice()),
    ] {
        privatefs::write_atomic(&fixture.profile.join(name), value)?;
    }
    privatefs::write_atomic(&fixture.node().join("running"), b"")?;
    let mut host = fixture.open(false)?;
    let marker = fixture.profile.join("fixture.mutation-written");
    let frozen_at = tokio::time::Instant::now();
    let real_started = std::time::Instant::now();
    let (result, process_path) = {
        let mutation = host.background(BackgroundAction::Update);
        tokio::pin!(mutation);
        loop {
            // A ready yield prevents paused Tokio from automatically advancing to the
            // deadline while the real OS process is starting or writing its marker.
            tokio::select! {
                biased;
                result = &mut mutation => bail!("Update completed before its committed marker: {result:?}"),
                _ = tokio::task::yield_now() => {}
            }
            if marker.try_exists()? {
                break;
            }
            assert!(
                real_started.elapsed() < Duration::from_secs(10),
                "child did not commit its mutation"
            );
        }
        assert_eq!(tokio::time::Instant::now(), frozen_at);
        let pid: u32 = std::fs::read_to_string(&marker)?.parse()?;
        let process_path = PathBuf::from(format!("/proc/{pid}"));
        assert!(pid > 0);
        assert!(
            process_path.exists(),
            "mutation child must still be alive after writing"
        );
        assert_eq!(
            privatefs::read_private_bounded(&marker, 32, "committed mutation PID")?,
            pid.to_string().as_bytes()
        );
        assert_eq!(
            privatefs::read_private_bounded(
                &fixture.profile.join("fixture.availability"),
                64,
                "committed runtime availability"
            )?,
            b"current"
        );

        // Exercise the unchanged 120-second Update deadline, rather than installing a
        // production timeout seam or waiting 120 seconds of wall-clock time.
        tokio::time::advance(Duration::from_secs(119)).await;
        tokio::select! {
            biased;
            result = &mut mutation => bail!("Update completed before its production deadline: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        let process_status = std::fs::read_to_string(process_path.join("status"))?;
        let state = process_status
            .lines()
            .find(|line| line.starts_with("State:"))
            .unwrap();
        assert!(
            !state.contains("Z (zombie)"),
            "child must remain live before the deadline"
        );
        assert_eq!(
            tokio::time::Instant::now() - frozen_at,
            Duration::from_secs(119)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let result = loop {
            // Keep time frozen at the expired deadline while production kill/wait and
            // the subsequent read-only Status use real OS process/pipe readiness.
            tokio::select! {
                biased;
                result = &mut mutation => break result,
                _ = tokio::task::yield_now() => {}
            }
            assert!(
                real_started.elapsed() < Duration::from_secs(10),
                "timeout cleanup or Status did not complete"
            );
        };
        (result, process_path)
    };
    assert_eq!(
        tokio::time::Instant::now() - frozen_at,
        Duration::from_secs(120)
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "后台节点操作超时，结果未知。请刷新状态核对，勿重复操作。"
    );
    assert!(
        !process_path.exists(),
        "production timeout must kill and reap its child"
    );
    let calls = std::fs::read_to_string(fixture.profile.join("fixture.calls"))?;
    let prefix = format!("desktop-node\t--profile-dir\t{}", fixture.profile.display());
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        vec![
            format!("{prefix}\tstatus"),
            format!("{prefix}\t--expected-device-id\t{}\tupdate", "a".repeat(64)),
            format!("{prefix}\tstatus"),
        ],
        "unknown mutation gets exactly one read-only reconciliation and is never repeated"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.profile.join("fixture.last-action"))?,
        "update"
    );
    let reconciled = host.snapshot().background.unwrap();
    assert_eq!(
        reconciled.runtime_update.availability,
        UpdateAvailability::Current
    );
    assert_eq!(reconciled.identity(), Some(fixture.identity()));
    assert_eq!(reconciled.state, ServiceState::Installed);
    assert_eq!(reconciled.service_running, Some(true));
    assert_eq!(reconciled.core_status, Some(NodeStatus::Running));
    Ok(())
}

#[tokio::test]
async fn legal_status_does_not_confirm_an_unfinished_background_action() -> Result<()> {
    for (action, argument, state, service_running, core_status) in [
        (
            BackgroundAction::Install,
            "install",
            "installed",
            Some(true),
            Some("unavailable"),
        ),
        (BackgroundAction::Install, "install", "conflict", None, None),
        (
            BackgroundAction::Install,
            "install",
            "partial",
            Some(true),
            Some("running"),
        ),
        (
            BackgroundAction::Start,
            "start",
            "installed",
            Some(true),
            Some("unavailable"),
        ),
        (BackgroundAction::Start, "start", "conflict", None, None),
        (
            BackgroundAction::Start,
            "start",
            "partial",
            Some(true),
            Some("running"),
        ),
        (
            BackgroundAction::Stop,
            "stop",
            "installed",
            Some(true),
            Some("running"),
        ),
        (
            BackgroundAction::Stop,
            "stop",
            "installed_stopped",
            Some(false),
            Some("unavailable"),
        ),
        (
            BackgroundAction::Stop,
            "stop",
            "partial",
            Some(false),
            Some("stopped"),
        ),
    ] {
        let fixture = Fixture::new()?;
        fixture.initialize()?;
        let mut host = fixture.open(false)?;
        if action != BackgroundAction::Install {
            host.background(BackgroundAction::Install).await?;
            if action == BackgroundAction::Start {
                host.background(BackgroundAction::Stop).await?;
            }
        }
        let wire = serde_json::json!({
            "state":state, "device_id":"a".repeat(64), "data_dir":fixture.node(),
            "service_id":"fixture.service", "managed_root":fixture.profile.join("managed"),
            "service_running":service_running, "core_status":core_status,
            "runtime_update":{"bundled_version":"2.0.0","installed_version":"2.0.0",
                "running_version":(service_running == Some(true) && core_status == Some("running"))
                    .then_some("2.0.0"),
                "availability":"current","recovery_required":false}
        });
        // This counterexample must remain a legitimate observation with the expected identity.
        let observed: host::ManagedNodeStatus = serde_json::from_value(wire.clone())?;
        observed.validate()?;
        assert_eq!(observed.identity(), Some(fixture.identity()));
        privatefs::write_atomic(
            &fixture.profile.join("fixture.mutation-status"),
            &serde_json::to_vec(&wire)?,
        )?;
        privatefs::write_atomic(&fixture.profile.join("fixture.calls"), b"")?;
        let error = host.background(action).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains(if action == BackgroundAction::Stop {
                    "后台节点停止尚未确认"
                } else {
                    "后台节点启动尚未确认"
                })
        );
        let calls = std::fs::read_to_string(fixture.profile.join("fixture.calls"))?;
        let calls: Vec<_> = calls.lines().collect();
        assert_eq!(
            calls.len(),
            3,
            "one preflight, one mutation, one reconciliation"
        );
        assert!(calls[0].ends_with("\tstatus"));
        assert!(calls[1].ends_with(&format!(
            "\t--expected-device-id\t{}\t{argument}",
            fixture.identity().device_id
        )));
        assert!(calls[2].ends_with("\tstatus"));
        assert_eq!(
            std::fs::read_to_string(fixture.profile.join("fixture.last-action"))?,
            argument
        );
        let reconciled = host.snapshot().background.unwrap();
        assert_eq!(reconciled.identity(), Some(fixture.identity()));
        assert_eq!(
            reconciled.core_status,
            Some(if action == BackgroundAction::Stop {
                NodeStatus::Stopped
            } else {
                NodeStatus::Running
            })
        );
    }
    Ok(())
}

#[tokio::test]
async fn unavailable_core_never_qualifies_for_stopped_recovery() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    let mut host = fixture.open(false)?;
    host.background(BackgroundAction::Install).await?;
    host.background(BackgroundAction::Stop).await?;
    let wire = serde_json::json!({
        "state":"installed_stopped", "device_id":"a".repeat(64), "data_dir":fixture.node(),
        "service_id":"fixture.service", "managed_root":fixture.profile.join("managed"),
        "service_running":false, "core_status":"unavailable",
        "runtime_update":{"bundled_version":"2.0.0","installed_version":"2.0.0",
            "running_version":null,"availability":"current","recovery_required":true}
    });
    privatefs::write_atomic(
        &fixture.profile.join("fixture.status"),
        &serde_json::to_vec(&wire)?,
    )?;
    let observed = host.background(BackgroundAction::Status).await?;
    assert_eq!(observed.core_status, Some(NodeStatus::Unavailable));
    assert!(host.background(BackgroundAction::Update).await.is_err());
    let calls = std::fs::read_to_string(fixture.profile.join("fixture.calls"))?;
    assert!(!calls.lines().any(|line| line.ends_with("\tupdate")));
    Ok(())
}

#[test]
fn activation_parser_queue_and_cancelled_file_save_preserve_intent() -> Result<()> {
    let ticket = "vibeface1abc123";
    let link = Activation::parse(&format!("vibeface://join/{ticket}")).unwrap();
    for invalid in [
        "vibeface://join/vibeface1ABC",
        "vibeface://join/vibeface1abc?path=/tmp",
        "file:///tmp/core",
        "vibeface://join/vibeface1",
        "vibeface1abc#runtime=x",
    ] {
        assert!(Activation::parse(invalid).is_none());
    }
    assert!(Activation::parse(&"a".repeat(4097)).is_none());
    let mut queue = ActivationQueue::default();
    assert!(queue.enqueue(link.clone()));
    assert!(queue.enqueue(link.clone()));
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.pop(), Some(link));
    for index in 0..32 {
        assert!(queue.enqueue(Activation::Join {
            ticket: format!("vibeface1{index}")
        }));
    }
    assert!(!queue.enqueue(Activation::Join {
        ticket: "vibeface1overflow".into()
    }));
    assert_eq!(
        save_selected(None, b"sealed recovery")?,
        SaveResult::Cancelled
    );
    let fixture = Fixture::new()?;
    let destination = fixture.root.join("recovery.json");
    assert_eq!(
        save_selected(Some(destination.clone()), b"sealed recovery")?,
        SaveResult::Saved(destination.clone())
    );
    assert_eq!(std::fs::read(destination)?, b"sealed recovery");
    Ok(())
}
#[tokio::test]
async fn single_instance_forwards_validated_activation_and_preserves_primary() -> Result<()> {
    let fixture = Fixture::new()?;
    let InstanceClaim::Primary(mut primary) = SingleInstance::claim(&fixture.profile, &[]).await?
    else {
        panic!("first claim must own singleton");
    };
    assert_eq!(primary.try_recv(), Some(Activation::Focus));
    let argv = vec![
        "--runtime=/untrusted".into(),
        "vibeface://join/vibeface1abc".into(),
    ];
    assert!(matches!(
        SingleInstance::claim(&fixture.profile, &argv).await?,
        InstanceClaim::Forwarded
    ));
    assert_eq!(
        primary.try_recv(),
        Some(Activation::Join {
            ticket: "vibeface1abc".into()
        })
    );
    assert!(!fixture.node().exists());
    Ok(())
}

#[tokio::test]
async fn service_output_overflow_never_triggers_a_mutation() -> Result<()> {
    let fixture = Fixture::new()?;
    privatefs::write_atomic(&fixture.profile.join("fixture.overflow"), b"")?;
    let mut host = fixture.open(false)?;
    assert!(host.background(BackgroundAction::Install).await.is_err());
    assert!(!fixture.profile.join("fixture.last-action").exists());
    assert!(!fixture.node().exists());
    Ok(())
}

#[tokio::test]
async fn native_host_parent_fixture() -> Result<()> {
    let Some(root) = std::env::var_os("VF_NATIVE_HOST_SYNTHETIC_PARENT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let mut host = DesktopHost::open(root.join("profile"), root.join("runtime"), false)?;
    host.start_owned().await?;
    privatefs::write_atomic(&root.join("parent.ready"), b"ready")?;
    std::future::pending::<()>().await;
    host.shutdown().await
}

#[tokio::test]
async fn killing_the_synthetic_gui_parent_delivers_eof_to_only_its_owned_child() -> Result<()> {
    let fixture = Fixture::new().context("create parent-death runtime fixture")?;
    fixture
        .initialize()
        .context("provision private parent-death Node fixture")?;
    let mut parent = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "host::native_tests::native_host_parent_fixture",
            "--nocapture",
        ])
        .env("VF_NATIVE_HOST_SYNTHETIC_PARENT", &fixture.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn the synthetic GUI parent test executable")?;
    wait_parent_ready(&mut parent, &fixture.root.join("parent.ready"))
        .await
        .context("synthetic GUI parent must start its owned Core before parent death")?;
    parent
        .start_kill()
        .context("kill only the held synthetic GUI parent")?;
    tokio::time::timeout(Duration::from_secs(3), parent.wait())
        .await
        .context("synthetic GUI parent did not exit after its held handle was killed")?
        .context("wait for the killed synthetic GUI parent")?;
    wait_file(&fixture.node().join("running"), false)
        .await
        .context("owned synthetic Core must publish EOF evidence before clearing running")?;
    let evidence = fixture.node().join("fixture.exited");
    assert_eq!(
        std::fs::read(&evidence).with_context(|| {
            format!(
                "read committed synthetic EOF evidence {}",
                evidence.display()
            )
        })?,
        b"eof"
    );
    assert!(fixture.node().join("identity.json").exists());
    Ok(())
}

#[tokio::test]
async fn unresponsive_owned_child_escalates_through_the_saved_handle() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.initialize()?;
    privatefs::write_atomic(&fixture.node().join("fixture.ignore-stdin"), b"")?;
    let core = fixture.runtime.join(if cfg!(windows) {
        "vibeface-core.exe"
    } else {
        "vibeface-core"
    });
    let mut owned = host::lifecycle::OwnedChild::start(&core, fixture.identity())?;
    wait_file(&fixture.node().join("running"), true).await?;
    owned.stop(Duration::from_millis(50)).await?;
    assert!(owned.poll()?.is_some());
    assert!(
        !fixture.node().join("fixture.exited").exists(),
        "an unresponsive child needs held-handle escalation"
    );
    Ok(())
}

#[tokio::test]
async fn single_instance_defers_busy_join_without_overwriting_or_losing_later_intents() -> Result<()>
{
    let fixture = Fixture::new()?;
    let InstanceClaim::Primary(mut primary) = SingleInstance::claim(&fixture.profile, &[]).await?
    else {
        panic!("the controlled primary must hold the activation queue");
    };
    assert_eq!(primary.try_recv(), Some(Activation::Focus));
    let first = Activation::Join {
        ticket: "vibeface1first".into(),
    };
    let second = Activation::Join {
        ticket: "vibeface1second".into(),
    };
    let third = Activation::Join {
        ticket: "vibeface1third".into(),
    };
    assert!(matches!(
        SingleInstance::claim(&fixture.profile, &["vibeface://join/vibeface1first".into()]).await?,
        InstanceClaim::Forwarded
    ));
    assert_eq!(primary.pending_capacity(), 31);
    assert!(primary.enqueue(second.clone()));
    assert!(primary.enqueue(third.clone()));
    for _ in 0..3 {
        let busy_join = primary.try_recv().unwrap();
        assert_eq!(busy_join, first);
        assert!(primary.defer(busy_join));
        assert_eq!(primary.pending_capacity(), 29);
    }
    // Finishing one form admits exactly one pending invitation, in original order.
    assert_eq!(primary.try_recv(), Some(first));
    let next = primary.try_recv().unwrap();
    assert_eq!(next, second);
    assert!(primary.defer(next));
    assert_eq!(primary.try_recv(), Some(second));
    assert_eq!(primary.try_recv(), Some(third));
    assert_eq!(primary.try_recv(), None);
    for index in 0..32 {
        assert!(primary.enqueue(Activation::Join {
            ticket: format!("vibeface1bounded{index}")
        }));
    }
    assert_eq!(primary.pending_capacity(), 0);
    let busy_join = primary.try_recv().unwrap();
    assert!(primary.defer(busy_join));
    assert_eq!(
        primary.pending_capacity(),
        0,
        "defer must not be deduplicated away or lose a tail entry"
    );
    for index in 0..32 {
        assert_eq!(
            primary.try_recv(),
            Some(Activation::Join {
                ticket: format!("vibeface1bounded{index}")
            })
        );
    }
    assert_eq!(primary.try_recv(), None);
    assert!(!fixture.node().exists());
    Ok(())
}

#[tokio::test]
async fn long_profiles_keep_short_private_activation_endpoints_and_isolated_instances() -> Result<()>
{
    let fixture = Fixture::new()?;
    let long_parent = fixture
        .root
        .join("long-profile-component".repeat(5))
        .join("second-profile-component".repeat(5));
    let first_profile = long_parent.join("first");
    let second_profile = long_parent.join("second");
    let first_profile = {
        privatefs::create_dir(&first_profile)?;
        std::fs::canonicalize(first_profile)?
    };
    let second_profile = {
        privatefs::create_dir(&second_profile)?;
        std::fs::canonicalize(second_profile)?
    };
    assert!(first_profile.to_string_lossy().len() > 250);
    let InstanceClaim::Primary(mut first) = SingleInstance::claim(&first_profile, &[]).await?
    else {
        panic!("first long profile must claim its own endpoint");
    };
    let InstanceClaim::Primary(mut second) = SingleInstance::claim(&second_profile, &[]).await?
    else {
        panic!("another profile must not attach to the first profile");
    };
    assert_eq!(first.try_recv(), Some(Activation::Focus));
    assert_eq!(second.try_recv(), Some(Activation::Focus));
    assert!(matches!(
        SingleInstance::claim(
            &first_profile,
            &["vibeface://join/vibeface1firstlong".into()]
        )
        .await?,
        InstanceClaim::Forwarded
    ));
    assert!(matches!(
        SingleInstance::claim(
            &second_profile,
            &["vibeface://join/vibeface1secondlong".into()]
        )
        .await?,
        InstanceClaim::Forwarded
    ));
    assert_eq!(
        first.try_recv(),
        Some(Activation::Join {
            ticket: "vibeface1firstlong".into()
        })
    );
    assert_eq!(
        second.try_recv(),
        Some(Activation::Join {
            ticket: "vibeface1secondlong".into()
        })
    );
    assert_eq!(first.try_recv(), None);
    assert_eq!(second.try_recv(), None);
    assert!(first_profile.join("desktop-instance.lock").is_file());
    assert!(second_profile.join("desktop-instance.lock").is_file());
    #[cfg(unix)]
    {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        for profile in [&first_profile, &second_profile] {
            let uid = std::fs::metadata(profile)?.uid();
            let directory = PathBuf::from(format!("/tmp/vibeface-desktop-{uid}"));
            privatefs::ensure_private(&directory, "controlled long-profile activation directory")?;
            let key = format!("{:x}", Sha256::digest(profile.to_string_lossy().as_bytes()));
            let endpoint = directory.join(format!("{}.sock", &key[..32]));
            use std::os::unix::ffi::OsStrExt;
            assert!(
                endpoint.as_os_str().as_bytes().len() < 104,
                "Darwin SUN_LEN must not depend on profile length"
            );
            assert!(
                std::fs::canonicalize(&endpoint)?
                    .as_os_str()
                    .as_bytes()
                    .len()
                    < 104,
                "canonical /private/tmp spelling must also fit Darwin SUN_LEN"
            );
            assert!(endpoint.exists());
            privatefs::ensure_private(&endpoint, "controlled long-profile socket")?;
        }
    }
    Ok(())
}
