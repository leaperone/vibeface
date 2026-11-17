//! Real producer boundary: the packaged Core --control process talks only to a private,
//! controlled IPC peer. No Node daemon, network listener, or provider is started.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use vibeface_bridge::platform::{
    ipc::{self, Endpoint, Listener},
    privatefs,
};
use vibeface_control::inspection::{Inspection, NodeStatus};
use vibeface_core::{
    config::{self, Config},
    crypto::LocalIdentity,
    maintenance,
};

struct Fixture {
    root: PathBuf,
    runtime: Option<PathBuf>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(runtime) = &self.runtime {
            let _ = std::fs::remove_dir_all(runtime);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
async fn control(request: &str) -> Result<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vibeface-core"))
        .arg("--control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("controlled Core stdin")?;
    stdin.write_all(request.as_bytes()).await?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await??;
    ensure!(
        output.status.success(),
        "controlled Core exited unsuccessfully"
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(envelope["ok"] == true, "controlled Core request failed");
    Ok(envelope["result"].clone())
}
async fn inspect(data: &Path) -> Result<Inspection> {
    let request = format!(
        "{{\"command\":\"inspect\",\"data_dir\":{}}}\n",
        serde_json::to_string(data)?
    );
    let wire = control(&request).await?;
    // Check the literal shape separately from the shared consumer's serde implementation.
    let expected = [
        "status",
        "device_id",
        "data_dir",
        "maintenance_socket",
        "listen",
        "local_listen",
        "public_url",
        "web_enabled",
        "http_enabled",
        "version",
        "write_state",
        "mesh",
        "peers",
        "core_version",
        "error",
    ];
    let object = wire.as_object().context("inspection object")?;
    assert_eq!(object.len(), expected.len());
    assert!(expected.iter().all(|key| object.contains_key(*key)));
    Inspection::from_value(wire)
}

#[tokio::test]
async fn real_core_inspection_distinguishes_stopped_running_and_unavailable() -> Result<()> {
    let root = std::env::temp_dir().join(format!("vf-inspection-process-{}", uuid::Uuid::new_v4()));
    privatefs::create_dir_new(&root)?;
    let mut fixture = Fixture {
        root: root.clone(),
        runtime: None,
    };
    let data = root.join("node");
    privatefs::create_dir_new(&data)?;
    let data = std::fs::canonicalize(data)?;
    let identity = LocalIdentity::generate();
    let device_id = identity.device_id()?;
    let mut configuration = Config::local("127.0.0.1:4790".parse()?, vec![data.clone()], false);
    configuration.network.mesh.enabled = false;
    configuration.network.mesh.network = "inspection-fixture".into();
    configuration.save(&data)?;
    config::private_write(&data.join("identity.json"), &serde_json::to_vec(&identity)?)?;
    config::private_write(&data.join("data-epoch.json"), &config::data_epoch_marker())?;
    let runtime = ipc::runtime_dir(&device_id[..32])?;
    ensure!(!runtime.exists(), "random fixture runtime must not exist");
    let endpoint = Endpoint::from_path(&maintenance::socket_path(&data)?)?;
    assert_eq!(
        endpoint.path(),
        Endpoint::in_private_dir(&runtime, "maintenance.sock")?.path()
    );
    let stopped = inspect(&data).await?;
    assert_eq!(stopped.status, NodeStatus::Stopped);
    assert_eq!(stopped.device_id, device_id);
    assert_eq!(stopped.data_dir, data);
    assert_eq!(stopped.maintenance_socket, endpoint.path());
    assert_eq!(stopped.core_version, env!("CARGO_PKG_VERSION"));
    assert!(stopped.version.is_none() && stopped.write_state.is_none() && stopped.peers.is_none());
    assert!(stopped.web_enabled && !stopped.http_enabled);
    assert!(
        !runtime.exists(),
        "inspection must not create the runtime directory"
    );
    privatefs::create_dir_new(&runtime)?;
    fixture.runtime = Some(runtime);
    let healthy = r#"{"protocol":1,"status":"healthy","health":{"device_id":"DEVICE","version":"0.0.1-hourly.2026110712","web_enabled":false,"http_enabled":true,"write_state":null,"peers":[],"mesh":{"records":12,"neighbors":3}},"error":null}"#.replace("DEVICE", &device_id);
    for (reply, expected_status) in [
        (healthy.clone(), NodeStatus::Running),
        (
            healthy.replace(&device_id, &"b".repeat(64)),
            NodeStatus::Unavailable,
        ),
        (
            healthy.replace("\"healthy\"", "\"committed\""),
            NodeStatus::Unavailable,
        ),
        (
            healthy.replace(
                "\"version\":\"0.0.1-hourly.2026110712\"",
                "\"version\":null",
            ),
            NodeStatus::Unavailable,
        ),
        (
            healthy.replace(
                "\"version\":\"0.0.1-hourly.2026110712\"",
                "\"version\":\"\"",
            ),
            NodeStatus::Unavailable,
        ),
        ("{broken".into(), NodeStatus::Unavailable),
    ] {
        let mut listener = Listener::bind(&endpoint).await?;
        let peer = tokio::spawn(async move {
            let mut stream = listener.accept().await?;
            stream.require_current_account("controlled inspection peer")?;
            let mut packet = Vec::new();
            loop {
                let byte = stream.read_u8().await?;
                packet.push(byte);
                if byte == b'\n' {
                    break;
                }
                ensure!(packet.len() < 4096, "bounded probe");
            }
            assert_eq!(packet, b"{\"protocol\":1,\"command\":\"probe\"}\n");
            stream.write_all(reply.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            Ok::<_, anyhow::Error>(())
        });
        let result = inspect(&data).await?;
        tokio::time::timeout(Duration::from_secs(10), peer).await???;
        assert_eq!(result.status, expected_status);
        assert_eq!(
            result.device_id, device_id,
            "keep the data directory's identity"
        );
        assert_eq!(result.data_dir, data);
        assert_eq!(result.maintenance_socket, endpoint.path());
        assert_eq!(result.listen.to_string(), "127.0.0.1:4790");
        assert_eq!(result.local_listen.to_string(), "127.0.0.1:4791");
        assert_eq!(result.public_url, "http://127.0.0.1:4790");
        assert!(
            !result.mesh.enabled,
            "mesh configuration is separate from runtime health"
        );
        assert_eq!(result.mesh.network, "inspection-fixture");
        if expected_status == NodeStatus::Running {
            assert_eq!(result.version.as_deref(), Some("0.0.1-hourly.2026110712"));
            assert_eq!(result.core_version, "0.0.1-hourly.2026110712");
            assert!(!result.web_enabled && result.http_enabled);
            assert!(result.peers.as_ref().unwrap().is_empty());
            assert_eq!(result.mesh.records, Some(12));
            assert_eq!(result.mesh.neighbors, Some(3));
            assert!(result.error.is_none());
        } else {
            assert_eq!(result.core_version, env!("CARGO_PKG_VERSION"));
            assert!(
                result.version.is_none() && result.write_state.is_none() && result.peers.is_none()
            );
            assert!(result.web_enabled && !result.http_enabled);
            assert!(result.mesh.records.is_none() && result.mesh.neighbors.is_none());
            assert!(result.error.as_ref().is_some_and(|e| !e.is_empty()));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        // A private, leftover socket with no listener is ConnectionRefused, hence Stopped.
        let leftover = UnixListener::bind(endpoint.path())?;
        drop(leftover);
        privatefs::restrict_file(endpoint.path())?;
        assert_eq!(inspect(&data).await?.status, NodeStatus::Stopped);
        std::fs::remove_file(endpoint.path())?;
        // A directory that is no longer private is unavailable even when the socket is absent.
        let runtime = fixture.runtime.as_ref().unwrap();
        std::fs::set_permissions(runtime, std::fs::Permissions::from_mode(0o755))?;
        assert_eq!(inspect(&data).await?.status, NodeStatus::Unavailable);
        privatefs::restrict_dir(runtime)?;
    }
    Ok(())
}
