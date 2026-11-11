//! Controlled protocol fixture: real Noise/TCP and actual private outbox, no Provider or user data.
#[path = "../src/app.rs"]
#[allow(dead_code, unused_imports)]
mod app;
#[path = "../src/host/mod.rs"]
#[allow(dead_code, unused_imports)]
mod host;
#[path = "../src/theme.rs"]
#[allow(dead_code)]
mod theme;
#[path = "../src/ui.rs"]
#[allow(dead_code)]
mod ui;
#[path = "../src/views/mod.rs"]
#[allow(dead_code, unused_imports)]
mod views;
#[path = "../src/widgets/mod.rs"]
#[allow(dead_code)]
mod widgets;
use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use vibeface_client::{
    Action, Client, ClientKey, Endpoint, Entry, OperationState,
    wire::{InputIntent, InputRequest, InputTarget},
};
use vibeface_secure::{
    Handshake, Identity,
    membership::{Genesis, IdentityKind, Member, NetworkState},
};
use views::{
    backend::{Backend, Command, Event},
    forms::Editor,
};

fn fixture_phase(phase: &'static str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "native fixture phase: {phase}");
}
async fn observed(
    receiver: &mut watch::Receiver<Arc<ui::Snapshot>>,
    predicate: impl Fn(&ui::Snapshot) -> bool,
) -> Result<Arc<ui::Snapshot>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = receiver.borrow_and_update().clone();
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            receiver
                .changed()
                .await
                .context("native snapshot stream ended")?;
        }
    })
    .await
    .context("native fixture snapshot timeout")?
}
fn connect(entry: &Entry) -> Command {
    Command::Save {
        editor: Editor::Connect,
        values: std::collections::BTreeMap::from([
            ("origin".into(), entry.origin.clone()),
            ("node_id".into(), entry.node_id.clone()),
            ("passphrase".into(), "controlled fixture password".into()),
            ("_submission_id".into(), "fixture-connect".into()),
        ]),
        conversation_id: None,
    }
}
fn host_at(profile: &PathBuf) -> Result<host::DesktopHost> {
    host::DesktopHost::open(profile.clone(), profile.join("runtime"), true)
}
async fn serve(
    listener: tokio::net::TcpListener,
    node: Identity,
    state: NetworkState,
    client_id: String,
    effects: Arc<AtomicUsize>,
) -> Result<()> {
    fixture_phase("server-await-connect");
    let (tcp, _) = listener.accept().await?;
    fixture_phase("server-accepted");
    let mut socket = tokio_tungstenite::accept_async(tcp).await?;
    ensure!(
        matches!(socket.next().await.context("connect")??, Message::Text(_)),
        "connect frame missing"
    );
    let mut handshake = Handshake::signed(false, &node, None, b"vibeface:client:v1")?;
    let Message::Binary(first) = socket.next().await.context("first Noise")?? else {
        anyhow::bail!("Noise frame missing")
    };
    handshake.read(&first)?;
    socket
        .send(Message::Binary(handshake.write(&[])?.into()))
        .await?;
    let Message::Binary(last) = socket.next().await.context("last Noise")?? else {
        anyhow::bail!("Noise frame missing")
    };
    handshake.read(&last)?;
    let mut channel = handshake.finish()?;
    fixture_phase("server-noise-ready");
    while let Some(frame) = socket.next().await {
        let Message::Binary(frame) = frame? else {
            continue;
        };
        let Some(bytes) = channel.open_record(&frame)? else {
            continue;
        };
        let request: Value = serde_json::from_slice(&bytes)?;
        let result = match request["method"].as_str() {
            Some("authenticate") => {
                json!({"network_id":state.genesis.network_id,"principal":{"source":"membership","device_id":client_id,"can_manage":true}})
            }
            Some("directory") => json!({"state":state,"now_ms":vibeface_protocol::now_ms()}),
            Some("write_state") => json!({"state":"writable","conflicts":[]}),
            Some("subscribe") => json!({"subscribed":true}),
            Some("rpc") => match request["params"]["method"].as_str() {
                Some("bots") => {
                    json!({"bots":[{"bot_id":"fixture-bot","conversation_id":"fixture-chat","name":"Fixture","models":{}}]})
                }
                Some("nodes") => {
                    json!({"nodes":[{"device_id":node.device_id(),"name":"Fixture Node","online":true,"revoked":false,"local":false}]})
                }
                Some("workspace_state") => {
                    json!({"tasks":[],"channels":[],"assignments":[],"inputs":[],"coverage":{"limit":100,"truncated":false}})
                }
                Some("questions") => json!({"questions":[]}),
                Some("notifications") => json!({"notifications":[]}),
                Some("conversation") => {
                    let mut conversation = json!({"conversation":{"conversation_id":"fixture-chat","title":"Fixture","owner":node.device_id(),"owner_epoch":1},"messages":[],"works":[],"scheduler":null,"questions":[],"sessions":[],"bot_settings":{"memory":[],"schedules":[],"skills":[],"memory_sync_conflicts":{"items":[],"has_more":false}}});
                    conversation["history_authority"] = json!("controlled-fixture-authority");
                    conversation["history"] = Value::Object(
                        vibeface_protocol::history::HistorySection::ALL
                            .into_iter()
                            .map(|section| {
                                (
                                    serde_json::to_value(section)
                                        .unwrap()
                                        .as_str()
                                        .unwrap()
                                        .to_owned(),
                                    json!({"next_cursor":null,"complete":true}),
                                )
                            })
                            .collect(),
                    );
                    conversation
                }
                Some("message") => {
                    effects.fetch_add(1, Ordering::SeqCst);
                    anyhow::bail!("fixture never authorized an online input")
                }
                _ => anyhow::bail!("unexpected controlled workspace read"),
            },
            _ => anyhow::bail!("unexpected controlled method"),
        };
        for frame in channel.seal_message(&serde_json::to_vec(
            &json!({"id":request["id"],"result":result}),
        )?)? {
            socket.send(Message::Binary(frame.into())).await?;
        }
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_backend_cold_offline_stages_original_input_in_actual_outbox() -> Result<()> {
    let profile = std::env::temp_dir().join(format!(
        "vibeface-native-cold-fixture-{}",
        uuid::Uuid::new_v4()
    ));
    fixture_phase("open-host");
    let initial_host = host_at(&profile)?;
    let directory = initial_host.client_storage_dir();
    fixture_phase("create-key");
    let mut key = ClientKey::create(&directory, "controlled fixture password")?;
    let node = Identity::generate()?;
    let client_id = key.device_id().to_owned();
    let member = Member::new(&client_id, IdentityKind::Client, "Fixture client", true)?;
    let genesis = Genesis::create(
        &Identity::generate()?,
        member,
        Member::node(&node, "Fixture Node")?,
    )?;
    let network = genesis.network_id.clone();
    let state = NetworkState::new(genesis)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let entry = Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: node.device_id(),
    };
    key.remember(&network, entry.clone())?;
    drop(key);
    let effects = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve(
        listener,
        node,
        state,
        client_id.clone(),
        effects.clone(),
    ));
    let base = format!("vibeface-native://{}", "1".repeat(64));
    fixture_phase("start-online-backend");
    let (tx, _events, mut snapshots, stop, task) = Backend::spawn(
        initial_host,
        &tokio::runtime::Handle::current(),
        base.clone(),
    );
    tx.send(connect(&entry)).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot.synced && snapshot.bots.len() == 1
    })
    .await?;
    tx.send(Command::Conversation("fixture-chat".into()))
        .await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .conversation
            .conversation
            .as_ref()
            .is_some_and(|conversation| conversation.conversation_id == "fixture-chat")
    })
    .await?;
    fixture_phase("online-conversation-read");
    let cache = directory
        .join("workspace-cache")
        .join(format!("{}.json", entry.node_id));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !tokio::fs::try_exists(&cache).await? {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    stop.send_replace(true);
    task.await?;
    drop(tx);
    server.abort();
    let _ = server.await;
    fixture_phase("start-cold-backend");
    let (tx, mut events, mut snapshots, stop, task) =
        Backend::spawn(host_at(&profile)?, &tokio::runtime::Handle::current(), base);
    tx.send(connect(&entry)).await?;
    let snapshot = observed(&mut snapshots, |snapshot| {
        snapshot.cached && snapshot.can_save_locally
    })
    .await?;
    assert!(!snapshot.online);
    assert!(!snapshot.synced);
    assert!(!snapshot.writable);
    assert_eq!(
        snapshot
            .conversation
            .conversation
            .as_ref()
            .unwrap()
            .conversation_id,
        "fixture-chat"
    );
    fixture_phase("stage-offline-input");
    tx.send(Command::Input {
        owner: None,
        conversation_id: "fixture-chat".into(),
        draft: ui::ComposerDraft {
            target: InputTarget::BotMain {
                bot_id: "fixture-bot".into(),
            },
            intent: InputIntent::Continue,
            text: "offline native input".into(),
            attachments: vec![],
            expected_revision: None,
        },
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.context("native events ended")?;
            let event = if let Event::Scoped { event, .. } = event {
                *event
            } else {
                event
            };
            match event {
                Event::InputStored(ack) => {
                    assert_eq!(ack.draft.text, "offline native input");
                    assert!(ack.owner.is_none());
                    return Ok::<_, anyhow::Error>(());
                }
                Event::Error(error) => anyhow::bail!("native staging failed: {error}"),
                _ => {}
            }
        }
    })
    .await??;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            directory.clone(),
            directory.join("outbox"),
            directory.join("outbox").join(&network),
            directory.join("outbox").join(&network).join(&client_id),
            directory.join("input-drafts"),
            directory.join("input-drafts").join(&network),
            directory
                .join("input-drafts")
                .join(&network)
                .join(&client_id),
        ] {
            assert_eq!(
                std::fs::metadata(&path)?.permissions().mode() & 0o777,
                0o700,
                "every app-owned identity/outbox/draft parent must be private"
            );
        }
    }
    stop.send_replace(true);
    task.await?;
    drop(tx);
    let key = Arc::new(ClientKey::open(&directory, "controlled fixture password")?);
    let client = Client::open_offline(
        key,
        Endpoint::Gateway(entry.clone()),
        directory.join("outbox").join(&network).join(&client_id),
    )
    .await?;
    let pending = client.pending().await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, OperationState::Prepared);
    assert_eq!(pending[0].sequence, Some(1));
    assert!(!client.is_online());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let mut drafts = tokio::fs::read_dir(
        directory
            .join("input-drafts")
            .join(&network)
            .join(&client_id),
    )
    .await?;
    let path = drafts
        .next_entry()
        .await?
        .context("durable native draft missing")?
        .path();
    let draft: Value = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    assert_eq!(draft["network_id"], network);
    assert_eq!(draft["client_id"], client_id);
    assert_eq!(draft["target_node"], entry.node_id);
    let input: InputRequest = serde_json::from_value(draft["input"].clone())?;
    assert_eq!(input.message_id, pending[0].id);
    assert_eq!(input.text, "offline native input");
    assert!(input.report_results);
    assert_eq!(draft["input"]["report_results"], json!(true));
    let restaged = client
        .stage(Action::Input {
            conversation_id: "fixture-chat".into(),
            input,
        })
        .await?;
    assert_eq!(restaged.id, pending[0].id);
    assert_eq!(restaged.sequence, Some(1));
    drop(client);
    // The enrolled metadata is removed only in this synthetic fixture. A signed old cache
    // still proves its exact identity/Node, while no membership or local grant is available.
    let key_path = directory.join("client.json");
    let mut key_file: Value = serde_json::from_slice(&std::fs::read(&key_path)?)?;
    key_file["network_id"] = Value::Null;
    vibeface_bridge::platform::privatefs::write_atomic(&key_path, &serde_json::to_vec(&key_file)?)?;
    let address = reqwest::Url::parse(&entry.origin)?;
    let no_network =
        tokio::net::TcpListener::bind(("127.0.0.1", address.port().context("fixture port")?))
            .await?;
    fixture_phase("start-private-view");
    let (tx, mut events, mut snapshots, stop, task) = Backend::spawn(
        host_at(&profile)?,
        &tokio::runtime::Handle::current(),
        "controlled-private-view".into(),
    );
    tx.send(connect(&entry)).await?;
    let private = observed(&mut snapshots, |snapshot| {
        snapshot.cached && snapshot.private_block.is_some()
    })
    .await?;
    assert!(!private.online);
    assert!(!private.can_stage_input());
    assert!(!private.can_control);
    assert!(!private.can_manage);
    assert_eq!(private.pending.len(), 1);
    assert_eq!(private.pending[0].target_node, entry.node_id);
    assert_eq!(
        private
            .conversation
            .conversation
            .as_ref()
            .unwrap()
            .conversation_id,
        "fixture-chat"
    );
    tx.send(Command::Input {
        owner: None,
        conversation_id: "fixture-chat".into(),
        draft: ui::ComposerDraft {
            target: InputTarget::BotMain {
                bot_id: "fixture-bot".into(),
            },
            intent: InputIntent::Continue,
            text: "must not stage".into(),
            attachments: vec![],
            expected_revision: None,
        },
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.context("private native events ended")?;
            match event {
                Event::Scoped { event, .. } => match *event {
                    Event::Error(_) => return Ok::<_, anyhow::Error>(()),
                    Event::InputStored(_) => {
                        anyhow::bail!("private viewer staged a business input")
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    })
    .await??;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), no_network.accept())
            .await
            .is_err(),
        "a private viewer must not contact the Node or request a grant"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    stop.send_replace(true);
    task.await?;
    drop(tx);
    std::fs::remove_dir_all(profile)?;
    Ok(())
}
