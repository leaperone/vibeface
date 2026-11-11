//! Real Noise client and actor. Delayed reads stay held until explicit test barriers release.
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
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use vibeface_client::ClientKey;
use vibeface_protocol::{
    history::HistorySection,
    workspace::{InputIntent, InputTarget},
};
use vibeface_secure::{
    Handshake, Identity,
    membership::{Genesis, IdentityKind, Member, NetworkState},
};
use views::{
    backend::{Backend, Command, Event},
    forms::Editor,
};

#[derive(Default)]
struct Probe {
    hold_home: AtomicBool,
    hold_history: AtomicBool,
    hold_delivery: AtomicBool,
    delivery_seen: Notify,
    release_delivery: Notify,
    home_seen: Notify,
    history_seen: Notify,
    release_home: Notify,
    release_history: Notify,
    history_released: Notify,
    stops: AtomicUsize,
    stop_seen: Notify,
    batches: AtomicUsize,
    head: Mutex<String>,
    conversation_override: Mutex<Option<Value>>,
    hold_conversation: AtomicBool,
    conversation_seen: Notify,
    release_conversation: Notify,
    retry_rejected: Notify,
    hold_work: AtomicBool,
    work_seen: Notify,
    release_work: Notify,
    work_reads: AtomicUsize,
}
fn conversation(id: &str, node: &str, head: &str) -> Value {
    let mut value = json!({
        "conversation":{"conversation_id":id,"owner":node,"owner_epoch":1},
        "history_authority":"fixture-authority",
        "messages":[{"message_id":if id=="chat"{"head"}else{"other-head"},"rank":"02","role":"assistant","text":head}],
        "works":[],"changes":[],"questions":[],"sessions":[],
        "scheduler":{"jobs":[{"trigger":"reply","state":"running","scope":{"kind":"main"}}],"scope_jobs":[],"unanswered":[]},
        "bot_settings":{"memory":[],"skills":[],"schedules":[],"memory_sync_conflicts":{"items":[],"has_more":false}}
    });
    value["history"] = Value::Object(
        HistorySection::ALL
            .into_iter()
            .map(|section| {
                let name = serde_json::to_value(section)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned();
                let coverage = if section == HistorySection::Messages {
                    json!({"next_cursor":"before-head","complete":false})
                } else {
                    json!({"next_cursor":null,"complete":true})
                };
                (name, coverage)
            })
            .collect(),
    );
    value
}
async fn serve(
    listener: tokio::net::TcpListener,
    node: Identity,
    state: NetworkState,
    client_id: String,
    probe: Arc<Probe>,
    mut pushes: mpsc::Receiver<Value>,
) -> Result<()> {
    let (tcp, _) = listener.accept().await?;
    let mut socket = tokio_tungstenite::accept_async(tcp).await?;
    ensure!(
        matches!(
            socket.next().await.context("connect frame")??,
            Message::Text(_)
        ),
        "gateway connect frame missing"
    );
    let mut handshake = Handshake::signed(false, &node, None, b"vibeface:client:v1")?;
    let Message::Binary(first) = socket.next().await.context("first Noise")?? else {
        anyhow::bail!("Noise frame missing");
    };
    handshake.read(&first)?;
    socket
        .send(Message::Binary(handshake.write(&[])?.into()))
        .await?;
    let Message::Binary(last) = socket.next().await.context("last Noise")?? else {
        anyhow::bail!("Noise frame missing");
    };
    handshake.read(&last)?;
    let mut channel = handshake.finish()?;
    let mut held_home: Option<Value> = None;
    let mut held_history: Option<Value> = None;
    let mut held_delivery: Option<Value> = None;
    let mut held_conversation: Option<Value> = None;
    let mut held_work: Option<Value> = None;
    loop {
        let response = tokio::select! {
            _=probe.release_home.notified(), if held_home.is_some()=>held_home.take().unwrap(),
            _=probe.release_conversation.notified(), if held_conversation.is_some()=>held_conversation.take().unwrap(),
            _=probe.release_work.notified(), if held_work.is_some()=>held_work.take().unwrap(),
            _=probe.release_delivery.notified(), if held_delivery.is_some()=>held_delivery.take().unwrap(),
            _=probe.release_history.notified(), if held_history.is_some()=>{
                probe.history_released.notify_one(); held_history.take().unwrap()
            },
            push=pushes.recv()=>{let Some(push)=push else{return Ok(())};push},
            frame=socket.next()=>{
                let Some(frame)=frame else{return Ok(())};
                let Message::Binary(frame)=frame? else{continue};
                let Some(bytes)=channel.open_record(&frame)? else{continue};
                let request:Value=serde_json::from_slice(&bytes)?;
                let method=request["params"]["method"].as_str().unwrap_or_else(||request["method"].as_str().unwrap_or(""));
                let result=match method {
                    "authenticate"=>json!({"network_id":state.genesis.network_id,"principal":{"source":"membership","device_id":client_id,"can_manage":true}}),
                    "directory"=>json!({"state":state,"now_ms":vibeface_protocol::now_ms()}),
                    "write_state"=>json!({"state":"writable","conflicts":[]}),
                    "subscribe"=>json!({"subscribed":true}),
                    "bots"=>{
                        let batch=probe.batches.fetch_add(1,Ordering::SeqCst)+1;
                        json!({"bots":[{"bot_id":"bot","conversation_id":"chat","name":format!("Batch {batch}"),"models":{}},{"bot_id":"other-bot","conversation_id":"other","name":"Other","models":{}}]})
                    },
                    "nodes"=>json!({"nodes":[{"device_id":node.device_id(),"name":"Fixture Node","online":true,"revoked":false,"local":false}]}),
                    "workspace_state"=>json!({"tasks":[],"channels":[],"assignments":[],"inputs":[]}),
                    "questions"=>json!({"questions":[]}),
                    "notifications"=>json!({"notifications":[]}),
                    "conversation"=>probe.conversation_override.lock().unwrap().clone().unwrap_or_else(||conversation(request["params"]["conversation_id"].as_str().unwrap(),&node.device_id(),&probe.head.lock().unwrap())),
                    "work_detail"=>{
                        let count=probe.work_reads.fetch_add(1,Ordering::SeqCst)+1;
                        json!({"work":{
                            "work_id":"held-work","conversation_id":"chat","target_node":node.device_id(),"trigger":"reply","dispatcher":node.device_id(),"kind":"codex",
                            "cwd":"/controlled","title":"Controlled work","prompt":"No Provider calls","call_ref":"controlled-call","model":null,"resolved":{"provider":"codex","model":null},
                            "continues":null,"parent":null,"thread":null,"phase":"finishing","summary":format!("record batch {count}"),"thread_id":"thread","turn_id":"turn",
                            "outcome":"completed","result_text":"complete controlled result","failure_reason":null,"failure":null,"state":"finished","target":null,"source_events":[]
                        },"coverage":{"source":"shared_history","complete":false}})
                    },
                    "conversation_history"=>{
                        ensure!(request["params"]["section"]=="messages","only message history fixture");
                        json!({"conversation_id":request["params"]["conversation_id"],"section":"messages","history_authority":"fixture-authority", "items":[{"message_id":"older","rank":"01","role":"user","text":"older full content"},{"message_id":"head","rank":"02","role":"assistant","text":"stale page head"}],"next_cursor":"before-older","complete":false})
                    },
                    "delivery"=>match request["params"]["op"].as_str() {
                        Some("status")=>json!({"node_peer":node.device_id(),"enabled":true,"charity_enabled":true,"paid_enabled":false}),
                        Some("list")=>json!({"orders":[]}),
                        Some("providers")=>json!({"providers":[]}),
                        _=>anyhow::bail!("the read fixture never authorizes delivery effects"),
                    },
                    "cancel_manager"=>{
                        probe.stops.fetch_add(1,Ordering::SeqCst);probe.stop_seen.notify_one();
                        json!({"cancel_requested":true,"job":{"conversation_id":request["params"]["conversation_id"],"trigger":request["params"]["trigger"],"state":{"state":"cancelling"}}})
                    },
                    "interrupt_work"=>{
                        probe.stops.fetch_add(1,Ordering::SeqCst);probe.stop_seen.notify_one();
                        json!({"interrupt_requested":true,"work_id":request["params"]["work_id"]})
                    },
                    // Keep the signed operation unknown so the durable draft remains inspectable.
                    "message"=>json!({"message_id":request["params"]["message_id"]}),
                    _=>anyhow::bail!("unexpected fixture method {method}"),
                };
                let response=json!({"id":request["id"],"result":result});
                if method=="conversation" && probe.hold_conversation.swap(false,Ordering::SeqCst) {
                    ensure!(held_conversation.is_none(),"only one raw conversation proof is held");
                    held_conversation=Some(response);probe.conversation_seen.notify_one();continue;
                }
                if method=="work_detail" && probe.hold_work.swap(false,Ordering::SeqCst) {
                    ensure!(held_work.is_none(),"same-key pushes must not spawn replacement reads");
                    held_work=Some(response);probe.work_seen.notify_one();continue;
                }
                if method=="bots" && probe.hold_home.swap(false,Ordering::SeqCst) {
                    ensure!(held_home.is_none(),"refresh demand must coalesce while held");
                    held_home=Some(response);probe.home_seen.notify_one();continue;
                }
                if method=="delivery" && request["params"]["op"]=="status" && probe.hold_delivery.swap(false,Ordering::SeqCst) {
                    ensure!(held_delivery.is_none(),"delivery refreshes must coalesce while held");
                    held_delivery=Some(response);probe.delivery_seen.notify_one();continue;
                }
                if method=="conversation_history" && probe.hold_history.swap(false,Ordering::SeqCst) {
                    ensure!(held_history.is_none(),"history requests must stay bounded");
                    held_history=Some(response);probe.history_seen.notify_one();continue;
                }
                response
            }
        };
        for frame in channel.seal_message(&serde_json::to_vec(&response)?)? {
            socket.send(Message::Binary(frame.into())).await?;
        }
    }
}
async fn observed(
    receiver: &mut watch::Receiver<Arc<ui::Snapshot>>,
    predicate: impl Fn(&ui::Snapshot) -> bool,
) -> Result<Arc<ui::Snapshot>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let value = receiver.borrow_and_update().clone();
            if predicate(&value) {
                return Ok(value);
            }
            receiver.changed().await.context("actor ended")?;
        }
    })
    .await
    .context("responsive actor state was not published")?
}
async fn signal(notify: &Notify) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .context("actor did not handle the operation while the read was held")
}
fn input(text: &str) -> Command {
    Command::Input {
        conversation_id: "chat".into(),
        owner: None,
        draft: ui::ComposerDraft {
            target: InputTarget::BotMain {
                bot_id: "bot".into(),
            },
            intent: InputIntent::Continue,
            text: text.into(),
            attachments: vec![],
            expected_revision: None,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delayed_reads_leave_stop_and_durable_input_responsive_and_fence_old_pages() -> Result<()> {
    let profile = std::env::temp_dir().join(format!("vf-responsive-{}", uuid::Uuid::new_v4()));
    let host = host::DesktopHost::open(profile.clone(), profile.join("runtime"), true)?;
    let directory = host.client_storage_dir();
    let mut key = ClientKey::create(&directory, "controlled fixture password")?;
    let node = Identity::generate()?;
    let client_id = key.device_id().to_owned();
    let genesis = Genesis::create(
        &Identity::generate()?,
        Member::new(&client_id, IdentityKind::Client, "Fixture client", true)?,
        Member::node(&node, "Fixture Node")?,
    )?;
    let network = genesis.network_id.clone();
    let state = NetworkState::new(genesis)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let entry = vibeface_client::Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: node.device_id(),
    };
    key.remember(&network, entry.clone())?;
    drop(key);
    let probe = Arc::new(Probe::default());
    *probe.head.lock().unwrap() = "initial head".into();
    let (push_tx, push_rx) = mpsc::channel(8);
    let server = tokio::spawn(serve(
        listener,
        node,
        state,
        client_id.clone(),
        probe.clone(),
        push_rx,
    ));
    let (tx, mut events, mut snapshots, stop, actor) = Backend::spawn(
        host,
        &tokio::runtime::Handle::current(),
        format!("vibeface-native://{}", "a".repeat(64)),
    );
    let event_probe = probe.clone();
    let drain = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let event = match event {
                Event::Scoped { event, .. } => *event,
                event => event,
            };
            if let Event::Error(error) = event {
                if error.contains("拥有者不可达") || error.contains("当前执行状态") {
                    event_probe.retry_rejected.notify_one();
                }
                eprintln!("controlled actor error: {error}");
            }
        }
    });
    tx.send(Command::Save {
        editor: Editor::Connect,
        values: BTreeMap::from([
            ("origin".into(), entry.origin.clone()),
            ("node_id".into(), entry.node_id.clone()),
            ("passphrase".into(), "controlled fixture password".into()),
        ]),
        conversation_id: None,
    })
    .await?;
    observed(&mut snapshots, |snapshot| {
        snapshot.online && snapshot.writable
    })
    .await?;
    tx.send(Command::Conversation("chat".into())).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .conversation
            .conversation
            .as_ref()
            .is_some_and(|c| c.conversation_id == "chat")
    })
    .await?;

    probe.hold_home.store(true, Ordering::SeqCst);
    tx.send(Command::Refresh).await?;
    signal(&probe.home_seen).await?;
    tx.send(Command::StopReply("chat".into())).await?;
    signal(&probe.stop_seen).await?;
    tx.send(input("input while Home is held")).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .drafts
            .iter()
            .any(|draft| draft.text == "input while Home is held")
    })
    .await?;
    assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
    // No release happened before both commands reached their actual production paths.
    probe.release_home.notify_one();
    observed(&mut snapshots, |snapshot| {
        snapshot.online
            && snapshot
                .bots
                .first()
                .is_some_and(|bot| bot.name != "Batch 1")
    })
    .await?;

    probe.hold_history.store(true, Ordering::SeqCst);
    tx.send(Command::LoadHistory {
        conversation_id: "chat".into(),
        section: HistorySection::Messages,
    })
    .await?;
    signal(&probe.history_seen).await?;
    *probe.head.lock().unwrap() = "pushed head".into();
    push_tx.send(json!({"type":"records","conversation_id":"chat","history_authority":"fixture-authority","messages":[{"message_id":"head","rank":"02","role":"assistant","text":"pushed head"}]})).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .conversation
            .messages
            .iter()
            .any(|message| message.message_id == "head" && message.text == "pushed head")
    })
    .await?;
    tx.send(Command::StopWork {
        conversation_id: "chat".into(),
        work_id: "work".into(),
    })
    .await?;
    signal(&probe.stop_seen).await?;
    tx.send(input("input while History is held")).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .drafts
            .iter()
            .any(|draft| draft.text == "input while History is held")
    })
    .await?;
    assert_eq!(probe.stops.load(Ordering::SeqCst), 2);
    probe.release_history.notify_one();
    signal(&probe.history_released).await?;
    let shown = observed(&mut snapshots, |snapshot| {
        snapshot.history_loading.is_none()
            && snapshot
                .conversation
                .messages
                .iter()
                .any(|message| message.message_id == "older")
    })
    .await?;
    assert_eq!(
        shown
            .conversation
            .messages
            .iter()
            .map(|message| (message.message_id.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        vec![("older", "older full content"), ("head", "pushed head")]
    );
    assert_eq!(
        shown.conversation.history["messages"]
            .next_cursor
            .as_deref(),
        Some("before-older")
    );

    tx.send(Command::Transfers).await?;
    observed(&mut snapshots, |snapshot| snapshot.delivery.enabled).await?;
    probe.hold_delivery.store(true, Ordering::SeqCst);
    tx.send(Command::Transfers).await?;
    signal(&probe.delivery_seen).await?;
    tx.send(Command::StopWork {
        conversation_id: "chat".into(),
        work_id: "delivery-view-work".into(),
    })
    .await?;
    signal(&probe.stop_seen).await?;
    tx.send(input("input while Delivery is held")).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .drafts
            .iter()
            .any(|draft| draft.text == "input while Delivery is held")
    })
    .await?;
    assert_eq!(probe.stops.load(Ordering::SeqCst), 3);
    probe.release_delivery.notify_one();

    probe.hold_history.store(true, Ordering::SeqCst);
    tx.send(Command::LoadHistory {
        conversation_id: "chat".into(),
        section: HistorySection::Messages,
    })
    .await?;
    signal(&probe.history_seen).await?;
    tx.send(Command::Conversation("other".into())).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot.history_loading.is_none()
            && snapshot
                .conversation
                .conversation
                .as_ref()
                .is_some_and(|c| c.conversation_id == "other")
    })
    .await?;
    probe.release_history.notify_one();
    signal(&probe.history_released).await?;
    let before = snapshots
        .borrow()
        .bots
        .first()
        .context("fresh Bot missing")?
        .name
        .clone();
    tx.send(Command::Refresh).await?;
    let shown = observed(&mut snapshots, |snapshot| {
        snapshot.bots.first().is_some_and(|bot| bot.name != before)
    })
    .await?;
    assert_eq!(
        shown
            .conversation
            .conversation
            .as_ref()
            .map(|c| c.conversation_id.as_str()),
        Some("other")
    );
    assert_eq!(
        shown
            .conversation
            .messages
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["other-head"]
    );
    // Repeated exact-record pushes must coalesce a slow active-record observation.
    tx.send(Command::Conversation("chat".into())).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .conversation
            .conversation
            .as_ref()
            .is_some_and(|c| c.conversation_id == "chat")
    })
    .await?;
    tx.send(Command::WatchRecord(views::backend::ActiveRecord::Work {
        conversation_id: "chat".into(),
        work_id: "held-work".into(),
    }))
    .await?;
    probe.hold_work.store(true, Ordering::SeqCst);
    tx.send(Command::WorkDetail {
        conversation_id: "chat".into(),
        work_id: "held-work".into(),
    })
    .await?;
    signal(&probe.work_seen).await?;
    for index in 0..4 {
        let question = format!("record-question-{index}");
        push_tx.send(json!({"type":"records","conversation_id":"chat","history_authority":"fixture-authority","questions":[{"question_id":question,"conversation_id":"chat","text":"controlled question","target":{"kind":"bot_main","bot_id":"bot"}}]})).await?;
        observed(&mut snapshots, |snapshot| {
            snapshot
                .conversation
                .questions
                .iter()
                .any(|row| row.question_id == question)
        })
        .await?;
    }
    assert_eq!(
        probe.work_reads.load(Ordering::SeqCst),
        1,
        "pushes must not abort and resubmit the held same-key read"
    );
    probe.release_work.notify_one();
    observed(&mut snapshots, |snapshot| {
        snapshot
            .work_detail
            .as_ref()
            .is_some_and(|detail| detail.work.summary == "record batch 2")
    })
    .await?;
    assert_eq!(
        probe.work_reads.load(Ordering::SeqCst),
        2,
        "one coalesced follow-up is sufficient"
    );
    tx.send(Command::WatchRecord(views::backend::ActiveRecord::None))
        .await?;

    // A fresh owner-unavailable response cannot borrow the retryable job retained for display.
    let mut old = conversation("chat", &entry.node_id, "old");
    old["messages"] = json!([{"message_id":"original","rank":"02","role":"user","text":"original controlled request"}]);
    old["scheduler"]["remote"] = json!({"owner":"remote-owner","reachable":true,"jobs":[{"trigger":"message:original","message_id":"original","state":"failed","retryable":true,"scope":{"kind":"main"}}],"scope_jobs":[],"memory_conflicts":[]});
    *probe.conversation_override.lock().unwrap() = Some(old.clone());
    tx.send(Command::Refresh).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .conversation
            .scheduler
            .retry_job_for_message("original")
            .is_some_and(|job| job.can_retry())
    })
    .await?;
    let mut unavailable = old;
    unavailable["scheduler"]["remote"] = json!({"owner":"remote-owner","reachable":false,"jobs":[],"scope_jobs":[],"memory_conflicts":[]});
    unavailable["history"]["owner_jobs"] =
        json!({"complete":false,"next_cursor":"unavailable-owner"});
    *probe.conversation_override.lock().unwrap() = Some(unavailable);
    probe.hold_conversation.store(true, Ordering::SeqCst);
    tx.send(Command::RetryReply {
        conversation_id: "chat".into(),
        message_id: "original".into(),
    })
    .await?;
    signal(&probe.conversation_seen).await?;
    push_tx.send(json!({"type":"records","conversation_id":"chat","history_authority":"fixture-authority","questions":[{"question_id":"proof-race","conversation_id":"chat","text":"newer metadata","target":{"kind":"bot_main","bot_id":"bot"}}]})).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot
            .conversation
            .questions
            .iter()
            .any(|row| row.question_id == "proof-race")
    })
    .await?;
    probe.release_conversation.notify_one();
    signal(&probe.retry_rejected).await?;
    let shown = snapshots.borrow().clone();
    assert!(
        !shown
            .drafts
            .iter()
            .any(|draft| draft.text.contains("original controlled request")),
        "merged display rows are never retry authority"
    );

    // A scope handoff waits for the canceled read to drop its exact Client lease.
    *probe.conversation_override.lock().unwrap() = None;
    probe.hold_work.store(true, Ordering::SeqCst);
    tx.send(Command::WorkDetail {
        conversation_id: "chat".into(),
        work_id: "held-work".into(),
    })
    .await?;
    signal(&probe.work_seen).await?;
    tx.send(Command::Disconnect).await?;
    observed(&mut snapshots, |snapshot| {
        snapshot.client_id.is_none() && snapshot.network.is_none()
    })
    .await?;
    let key = Arc::new(ClientKey::open(&directory, "controlled fixture password")?);
    let reopened = vibeface_client::Client::open_offline(
        key,
        vibeface_client::Endpoint::Gateway(entry.clone()),
        directory.join("outbox").join(&network).join(&client_id),
    )
    .await?;
    assert_eq!(reopened.client_id(), client_id);
    reopened.close();
    drop(reopened);
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), actor).await??;
    drain.abort();
    server.abort();
    drop(push_tx);
    std::fs::remove_dir_all(profile)?;
    Ok(())
}
