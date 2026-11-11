use crate::host::fixture_rustc;

use super::*;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use vibeface_bridge::platform::{
    ipc::{self, Listener},
    privatefs,
};
use vibeface_client::Outlet;
use vibeface_control::maintenance::{
    MaintenanceCommand, MaintenanceRequest, MaintenanceResponse, MaintenanceStatus, PROTOCOL,
};

fn provider_change() -> NodeOperatorChange {
    NodeOperatorChange::ProviderSettings(ProviderSettings {
        provider: Provider::Codex,
        bot: None,
        execution: Some(false),
        execution_model: None,
        bin: None,
        app_server: None,
        connect_local: false,
        env: BTreeMap::new(),
    })
}

fn triage_change(secret: SecretChange) -> NodeOperatorChange {
    NodeOperatorChange::Triage(TriageSettings {
        default_outlet: Outlet::Record,
        projects: vec![],
        webhook_url: None,
        scan_codex_sessions: false,
        codex_sessions: None,
        webhook_secret: secret,
    })
}

#[test]
fn operator_wire_has_no_governance_and_preserves_omitted_settings() -> Result<()> {
    for method in [
        "bootstrap",
        "invite",
        "approve",
        "grant",
        "revoke",
        "recover",
        "recovery_state",
        "join",
        "join_withdraw",
        "subscribe",
        "input",
        "workspace_command",
        "delivery",
        "confirm_writable",
    ] {
        assert!(!READ_METHODS.contains(&method));
        assert!(!CHANGE_METHODS.contains(&method));
    }
    assert!(NodeOperatorChange::try_from(Action::ConfirmWritable).is_err());
    assert!(
        NodeOperatorChange::try_from(Action::DeleteBot {
            conversation_id: "other-scope".into()
        })
        .is_err()
    );
    let (method, params) = provider_change().wire()?;
    assert_eq!(method, "configure_provider");
    assert_eq!(params["execution"], false);
    for field in ["bot", "execution_model", "bin", "app_server"] {
        assert!(
            params.get(field).is_none(),
            "an omitted provider field must remain untouched"
        );
    }
    assert!(
        triage_change(SecretChange::Keep)
            .wire()?
            .1
            .get("webhook_secret")
            .is_none()
    );
    assert!(triage_change(SecretChange::Clear).wire()?.1["webhook_secret"].is_null());
    let error = sanitize_call_error(
        maintenance::Refused("synthetic-never-log".into()).into(),
        true,
    );
    assert!(error.is::<OperatorOutcomeUnknown>());
    assert!(!format!("{error:#}").contains("synthetic-never-log"));
    Ok(())
}

#[derive(Default)]
struct State {
    methods: Vec<String>,
    config: Value,
    drop_next_change: bool,
    change_identity_after_write: bool,
}

struct Fixture {
    root: PathBuf,
    profile: PathBuf,
    runtime: PathBuf,
    socket_dir: PathBuf,
    socket: PathBuf,
    identity: NodeIdentity,
    state: Arc<Mutex<State>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let key = uuid::Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!("vfo-{}", &key[..12]));
        let profile = root.join("profile");
        let runtime = root.join("runtime");
        let node = profile.join("node");
        for path in [&root, &profile, &runtime, &node] {
            privatefs::create_dir(path)?;
        }
        let identity = NodeIdentity {
            data_dir: node,
            device_id: format!("{key}{key}"),
        };
        privatefs::write_atomic(&identity.data_dir.join("config.json"), b"{}")?;
        privatefs::write_atomic(&identity.data_dir.join("identity.json"), b"{}")?;
        let source = root.join("fake_core.rs");
        std::fs::write(&source, FAKE_CORE)?;
        let core = runtime.join(if cfg!(windows) {
            "vibeface-core.exe"
        } else {
            "vibeface-core"
        });
        let status = fixture_rustc::command()
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(core)
            .status()?;
        ensure!(status.success(), "compile the isolated inspector fixture");
        let socket_dir = ipc::runtime_dir(&format!("operatortest{key}"))?;
        privatefs::create_dir(&socket_dir)?;
        let endpoint = Endpoint::in_private_dir(&socket_dir, "maintenance.sock")?;
        let socket = endpoint.path().to_owned();
        let mut listener = Listener::bind(&endpoint).await?;
        let state = Arc::new(Mutex::new(State {
            config: json!({
                "roots": ["/kept"], "manager": null, "untouched": "kept",
                "providers": {"codex": {"execution": true, "execution_model": "kept-model"}},
                "webhook_secret": "synthetic-private-webhook"
            }),
            ..Default::default()
        }));
        let service_state = state.clone();
        let service_inspect = profile.join("fixture-inspect.json");
        let server = tokio::spawn(async move {
            loop {
                let Ok(stream) = listener.accept().await else {
                    return;
                };
                if stream
                    .require_current_account("controlled owner operator")
                    .is_err()
                {
                    continue;
                }
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                if stream.read_line(&mut line).await.is_err() {
                    return;
                }
                let request: MaintenanceRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(request.protocol, PROTOCOL);
                let MaintenanceCommand::Client { method, params } = request.command else {
                    panic!("only owner operator requests are accepted");
                };
                let result = {
                    let mut state = service_state.lock().unwrap();
                    state.methods.push(method.clone());
                    match method.as_str() {
                        "configure_provider" => {
                            for (key, value) in params.as_object().unwrap() {
                                if key != "provider" && key != "env" && key != "connect_local" {
                                    state.config["providers"]["codex"][key] = value.clone();
                                }
                            }
                        }
                        "configure_triage" => {
                            if let Some(secret) = params.get("webhook_secret") {
                                state.config["webhook_secret"] = secret.clone();
                            }
                        }
                        _ => {}
                    }
                    if state.change_identity_after_write
                        && CHANGE_METHODS.contains(&method.as_str())
                    {
                        state.change_identity_after_write = false;
                        let mut inspected: Value =
                            serde_json::from_slice(&std::fs::read(&service_inspect).unwrap())
                                .unwrap();
                        inspected["result"]["device_id"] = Value::String("d".repeat(64));
                        privatefs::write_atomic(
                            &service_inspect,
                            &serde_json::to_vec(&inspected).unwrap(),
                        )
                        .unwrap();
                    }
                    if state.drop_next_change && CHANGE_METHODS.contains(&method.as_str()) {
                        state.drop_next_change = false;
                        None
                    } else {
                        let (status, error, result) = match method.as_str() {
                            "diagnostics" => (
                                MaintenanceStatus::Error,
                                Some("personal network principal required".into()),
                                None,
                            ),
                            "node_settings" => (
                                MaintenanceStatus::Ready,
                                None,
                                Some(
                                    json!({"roots":state.config["roots"], "manager":state.config["manager"]}),
                                ),
                            ),
                            "create_decision_token" => (
                                MaintenanceStatus::Ready,
                                None,
                                Some(
                                    json!({"token_id":"fixture-token", "token":"synthetic-copy-only-secret"}),
                                ),
                            ),
                            _ => (
                                MaintenanceStatus::Ready,
                                None,
                                Some(json!({"status":"saved"})),
                            ),
                        };
                        Some(MaintenanceResponse {
                            protocol: PROTOCOL,
                            status,
                            health: None,
                            error,
                            wallet: None,
                            delivery: None,
                            join: None,
                            result,
                        })
                    }
                };
                if let Some(result) = result {
                    let mut bytes = serde_json::to_vec(&result).unwrap();
                    bytes.push(b'\n');
                    let _ = stream.get_mut().write_all(&bytes).await;
                }
            }
        });
        let fixture = Self {
            root,
            profile,
            runtime,
            socket_dir,
            socket,
            identity,
            state,
            server,
        };
        fixture.inspect(
            &fixture.identity.device_id,
            &fixture.identity.data_dir,
            "running",
        )?;
        Ok(fixture)
    }

    fn inspect(&self, device_id: &str, data_dir: &Path, status: &str) -> Result<()> {
        privatefs::write_atomic(
            &self.profile.join("fixture-inspect.json"),
            &serde_json::to_vec(&json!({
                "ok": true,
                "result": {
                    "data_dir":data_dir, "device_id":device_id, "status":status,
                    "local_listen":"127.0.0.1:4791", "listen":"127.0.0.1:4790", "web_enabled":true,
                    "core_version":"0.0.1", "maintenance_socket":self.socket, "error":null
                }
            }))?,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.socket_dir);
    }
}

const FAKE_CORE: &str = r#"
use std::{env, fs, io::{self, Read}};
fn main() {
    assert_eq!(env::args().nth(1).as_deref(), Some("--control"));
    let mut input=String::new(); io::stdin().take(1048576).read_to_string(&mut input).unwrap();
    assert!(input.contains("\"command\":\"inspect\""), "fixture refuses all mutating Core commands");
    let profile=env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("profile");
    println!("{}",fs::read_to_string(profile.join("fixture-inspect.json")).unwrap());
}
"#;

#[tokio::test]
async fn owner_operator_checks_selection_identity_and_preserves_unknown_writes() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut host = DesktopHost::open(fixture.profile.clone(), fixture.runtime.clone(), true)?;
    assert!(
        host.operator_read(&fixture.identity, NodeOperatorRead::NodeSettings)
            .await
            .is_err()
    );
    assert!(fixture.state.lock().unwrap().methods.is_empty());
    host.select_node(&fixture.identity).await?;
    let settings = host
        .operator_read(&fixture.identity, NodeOperatorRead::NodeSettings)
        .await?;
    assert_eq!(settings["roots"], json!(["/kept"]));
    assert!(
        !fixture
            .runtime
            .join(if cfg!(windows) {
                "vibeface.exe"
            } else {
                "vibeface"
            })
            .exists()
    );
    host.operator_change(&fixture.identity, provider_change())
        .await?;
    host.operator_change(&fixture.identity, triage_change(SecretChange::Keep))
        .await?;
    {
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.config["untouched"], "kept");
        assert_eq!(state.config["roots"], json!(["/kept"]));
        assert_eq!(
            state.config["providers"]["codex"]["execution_model"],
            "kept-model"
        );
        assert_eq!(state.config["webhook_secret"], "synthetic-private-webhook");
    }
    let before = fixture.state.lock().unwrap().methods.len();
    fixture.inspect(&"d".repeat(64), &fixture.identity.data_dir, "running")?;
    assert!(
        host.operator_change(&fixture.identity, provider_change())
            .await
            .is_err()
    );
    fixture.inspect(
        &fixture.identity.device_id,
        &fixture.identity.data_dir,
        "stopped",
    )?;
    assert!(
        host.operator_read(&fixture.identity, NodeOperatorRead::NodeSettings)
            .await
            .is_err()
    );
    fixture.inspect(&fixture.identity.device_id, &fixture.profile, "running")?;
    assert!(
        host.operator_read(&fixture.identity, NodeOperatorRead::NodeSettings)
            .await
            .is_err()
    );
    fixture.inspect(
        &fixture.identity.device_id,
        &fixture.identity.data_dir,
        "running",
    )?;
    let config = fixture.identity.data_dir.join("config.json");
    let saved_config = fixture.identity.data_dir.join("saved-config.json");
    std::fs::rename(&config, &saved_config)?;
    assert!(
        host.operator_read(&fixture.identity, NodeOperatorRead::NodeSettings)
            .await
            .is_err()
    );
    std::fs::rename(saved_config, config)?;
    assert_eq!(fixture.state.lock().unwrap().methods.len(), before);
    let error = host
        .operator_read(
            &fixture.identity,
            NodeOperatorRead::Diagnostics {
                since_ms: None,
                until_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.is::<OperatorRefused>(),
        "Core's pre-network diagnostics refusal stays authoritative"
    );
    fixture.state.lock().unwrap().drop_next_change = true;
    let before = fixture
        .state
        .lock()
        .unwrap()
        .methods
        .iter()
        .filter(|method| method.as_str() == "configure_provider")
        .count();
    let error = host
        .operator_change(&fixture.identity, provider_change())
        .await
        .unwrap_err();
    assert!(error.is::<OperatorOutcomeUnknown>());
    let after = fixture
        .state
        .lock()
        .unwrap()
        .methods
        .iter()
        .filter(|method| method.as_str() == "configure_provider")
        .count();
    assert_eq!(
        after,
        before + 1,
        "a lost config acknowledgement is never retried"
    );
    fixture.state.lock().unwrap().change_identity_after_write = true;
    let error = host
        .operator_change(&fixture.identity, provider_change())
        .await
        .unwrap_err();
    assert!(
        error.is::<OperatorOutcomeUnknown>(),
        "post-write identity changes cannot report success"
    );
    fixture.inspect(
        &fixture.identity.device_id,
        &fixture.identity.data_dir,
        "running",
    )?;
    let token = host
        .operator_change(
            &fixture.identity,
            NodeOperatorChange::CreateDecisionToken {
                label: "explicit copy".into(),
            },
        )
        .await?;
    assert_eq!(token["token"], "synthetic-copy-only-secret");
    let snapshot = serde_json::to_string(&host.snapshot())?;
    assert!(!snapshot.contains("synthetic-copy-only-secret"));
    assert!(!snapshot.contains("synthetic-private-webhook"));
    assert!(host.snapshot().owned.is_none());
    assert!(!fixture.identity.data_dir.join("node.sqlite3").exists());
    Ok(())
}
