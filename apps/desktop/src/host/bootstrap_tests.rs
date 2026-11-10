use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};
use vibeface_bridge::platform::privatefs;

const NODE_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const LAUNCHER_CAPABILITY: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BOOTSTRAP_CAPABILITY: &str =
    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const ORIGIN: &str = "http://127.0.0.1:4791";

fn identity(path: &Path) -> NodeIdentity {
    NodeIdentity {
        data_dir: path.into(),
        device_id: NODE_ID.into(),
    }
}

fn launcher() -> LocalHttpCredential {
    LocalHttpCredential {
        origin: ORIGIN.into(),
        node_id: NODE_ID.into(),
        capability: LAUNCHER_CAPABILITY.into(),
    }
}

fn uninitialized() -> Value {
    json!({
        "kind": "uninitialized",
        "invitation": {
            "url": ORIGIN,
            "device_id": NODE_ID,
            "capability": BOOTSTRAP_CAPABILITY
        }
    })
}

#[test]
fn bootstrap_replies_preserve_network_state_and_redact_authority() -> Result<()> {
    let expected = identity(Path::new("/controlled/node"));
    let launcher = launcher();
    let invitation = parse_invitation(uninitialized(), &expected, &launcher)?;
    assert_eq!(invitation.origin, ORIGIN);
    assert_eq!(invitation.node_id, NODE_ID);
    assert_eq!(invitation.capability.as_str(), BOOTSTRAP_CAPABILITY);
    let debug = format!("{invitation:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains(BOOTSTRAP_CAPABILITY));
    assert!(!debug.contains(LAUNCHER_CAPABILITY));

    let initialized = json!({
        "kind": "initialized",
        "entry": { "address": ORIGIN, "device_id": NODE_ID },
        "removed": false,
        "local_launch": { "url": ORIGIN, "device_id": NODE_ID, "token": "not-a-bootstrap" }
    });
    let error = parse_invitation(initialized, &expected, &launcher).unwrap_err();
    assert!(error.to_string().contains("已属于个人网络"));
    for initialized in [false, true] {
        assert!(
            parse_invitation(
                json!({ "kind": "owner_required", "initialized": initialized }),
                &expected,
                &launcher,
            )
            .is_err()
        );
    }
    for (field, wrong) in [
        ("url", "https://remote.example".into()),
        ("url", "http://127.0.0.1:4792".into()),
        ("url", "http://127.0.0.1:4791/path".into()),
        ("url", "http://user@127.0.0.1:4791".into()),
        ("url", "http://127.0.0.1:4791/?runtime=evil".into()),
        ("device_id", "d".repeat(64)),
        ("capability", LAUNCHER_CAPABILITY.into()),
        ("capability", "invalid-capability".into()),
    ] {
        let mut reply = uninitialized();
        reply["invitation"][field] = Value::String(wrong);
        let error = parse_invitation(reply, &expected, &launcher).unwrap_err();
        assert!(!format!("{error:#}").contains(BOOTSTRAP_CAPABILITY));
        assert!(!format!("{error:#}").contains(LAUNCHER_CAPABILITY));
    }
    Ok(())
}

struct Fixture {
    root: PathBuf,
    profile: PathBuf,
    runtime: PathBuf,
    node: NodeIdentity,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("vf-bootstrap-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile");
        let runtime = root.join("runtime");
        let node = identity(&profile.join("node"));
        privatefs::create_dir(&root)?;
        privatefs::create_dir(&profile)?;
        privatefs::create_dir(&runtime)?;
        privatefs::create_dir(&node.data_dir)?;
        privatefs::write_atomic(&node.data_dir.join("config.json"), b"{}")?;
        privatefs::write_atomic(&node.data_dir.join("identity.json"), b"{}")?;
        privatefs::write_atomic(
            &node.data_dir.join("local-access.json"),
            &serde_json::to_vec(&json!({
                "origin": ORIGIN,
                "node_id": NODE_ID,
                "capability": LAUNCHER_CAPABILITY
            }))?,
        )?;
        let source = root.join("fake_core.rs");
        std::fs::write(&source, FAKE_CORE)?;
        let core = runtime.join(if cfg!(windows) {
            "vibeface-core.exe"
        } else {
            "vibeface-core"
        });
        let status = std::process::Command::new(
            std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
        )
        .args(["--edition=2024"])
        .arg(&source)
        .arg("-o")
        .arg(&core)
        .status()?;
        ensure!(status.success(), "compile the controlled Core fixture");
        let fixture = Self {
            root,
            profile,
            runtime,
            node,
        };
        fixture.inspect(NODE_ID, &fixture.node.data_dir, "running")?;
        fixture.setup(uninitialized())?;
        Ok(fixture)
    }

    fn inspect(&self, node_id: &str, data_dir: &Path, status: &str) -> Result<()> {
        privatefs::write_atomic(
            &self.profile.join("fixture-inspect.json"),
            &serde_json::to_vec(&json!({
                "ok": true,
                "result": {
                    "data_dir": data_dir,
                    "device_id": node_id,
                    "status": status,
                    "local_listen": "127.0.0.1:4791",
                    "listen": "127.0.0.1:4790",
                    "web_enabled": true,
                    "core_version": "0.0.1",
                    "error": null
                }
            }))?,
        )
    }

    fn setup(&self, value: Value) -> Result<()> {
        privatefs::write_atomic(
            &self.profile.join("fixture-setup.json"),
            &serde_json::to_vec(&json!({ "ok": true, "result": value }))?,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const FAKE_CORE: &str = r#"
use std::{env, fs, io::{self, Read, Write}};
fn main() {
    assert_eq!(env::args().nth(1).as_deref(), Some("--control"));
    let profile = env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("profile");
    let mut input = String::new();
    io::stdin().take(1024 * 1024).read_to_string(&mut input).unwrap();
    let (command, response) = if input.contains("\"command\":\"inspect\"") {
        ("inspect", "fixture-inspect.json")
    } else if input.contains("\"command\":\"local_setup\"") {
        ("local_setup", "fixture-setup.json")
    } else {
        panic!("unexpected command; fixture never initializes or starts a Node");
    };
    let mut log = fs::OpenOptions::new().create(true).append(true).open(profile.join("fixture-commands")).unwrap();
    writeln!(log, "{command}").unwrap();
    println!("{}", fs::read_to_string(profile.join(response)).unwrap());
}
"#;

#[tokio::test]
async fn selected_node_bootstrap_uses_local_setup_without_initializing_or_leaking() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut host = DesktopHost::open(fixture.profile.clone(), fixture.runtime.clone(), true)?;
    assert!(host.bootstrap_invitation(&fixture.node).await.is_err());
    assert!(!fixture.profile.join("fixture-commands").exists());
    host.select_node(&fixture.node).await?;
    let invitation = host.bootstrap_invitation(&fixture.node).await?;
    assert_eq!(invitation.capability.as_str(), BOOTSTRAP_CAPABILITY);
    let snapshot = serde_json::to_string(&host.snapshot())?;
    assert!(!snapshot.contains(BOOTSTRAP_CAPABILITY));
    assert!(!snapshot.contains(LAUNCHER_CAPABILITY));
    let settings = std::fs::read_to_string(fixture.profile.join("native-desktop-settings.json"))?;
    assert!(!settings.contains(BOOTSTRAP_CAPABILITY));
    assert!(!settings.contains(LAUNCHER_CAPABILITY));
    fixture.setup(json!({
        "kind": "initialized",
        "entry": { "address": ORIGIN, "device_id": NODE_ID },
        "removed": false
    }))?;
    assert!(
        host.bootstrap_invitation(&fixture.node)
            .await
            .unwrap_err()
            .to_string()
            .contains("已属于个人网络")
    );
    fixture.setup(uninitialized())?;
    fixture.inspect(&"d".repeat(64), &fixture.node.data_dir, "running")?;
    assert!(host.bootstrap_invitation(&fixture.node).await.is_err());
    fixture.inspect(NODE_ID, &fixture.profile, "running")?;
    assert!(host.bootstrap_invitation(&fixture.node).await.is_err());
    fixture.inspect(NODE_ID, &fixture.node.data_dir, "stopped")?;
    assert!(host.bootstrap_invitation(&fixture.node).await.is_err());
    fixture.inspect(NODE_ID, &fixture.node.data_dir, "running")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let credential = fixture.node.data_dir.join("local-access.json");
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o644))?;
        assert!(host.bootstrap_invitation(&fixture.node).await.is_err());
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600))?;
        let credential_copy = fixture.node.data_dir.join("fixture-launcher.json");
        std::fs::rename(&credential, &credential_copy)?;
        symlink(&credential_copy, &credential)?;
        assert!(host.bootstrap_invitation(&fixture.node).await.is_err());
    }
    let commands = std::fs::read_to_string(fixture.profile.join("fixture-commands"))?;
    assert!(commands.lines().any(|command| command == "local_setup"));
    assert!(commands.lines().all(|command| matches!(command, "inspect" | "local_setup")));
    assert!(!commands.contains(BOOTSTRAP_CAPABILITY));
    assert!(!commands.contains(LAUNCHER_CAPABILITY));
    assert!(host.snapshot().owned.is_none());
    assert!(!fixture.node.data_dir.join("node.sqlite3").exists());
    Ok(())
}
