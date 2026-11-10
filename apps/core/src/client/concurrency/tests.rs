use super::*;
use crate::{
    config::{self, Config},
    identity::{Genesis, Revocation},
    providers::Provider,
    runtime::test_support::initialize_node,
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::protocol::Role};

const DEADLINE: Duration = Duration::from_secs(2);

fn fixture_work_id() -> String {
    crate::work::WorkSpec::id("scope", "message:fixture-message", "fixture-call")
}

struct HeldWriter {
    release: Option<std::sync::mpsc::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl HeldWriter {
    async fn new(node: &Arc<Runtime>) -> Result<Self> {
        Self::after(node, |_| Ok(())).await
    }

    async fn after(
        node: &Arc<Runtime>,
        before_barrier: impl FnOnce(&mut crate::store::Store) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let node = node.clone();
        let task = tokio::spawn(async move {
            node.store.call("fixture_held_writer", move |store| {
                before_barrier(store)?;
                let _ = entered.send(());
                wait.recv().context("writer barrier was abandoned")?;
                Ok(())
            }).await
        });
        let held = Self { release: Some(release), task: Some(task) };
        tokio::time::timeout(DEADLINE, ready).await??;
        Ok(held)
    }

    async fn finish(mut self) -> Result<()> {
        if let Some(release) = self.release.take() { let _ = release.send(()); }
        tokio::time::timeout(DEADLINE, self.task.take().context("writer task missing")?).await???;
        Ok(())
    }
}

impl Drop for HeldWriter {
    fn drop(&mut self) {
        // An assertion failure must not leave the dedicated writer thread parked forever.
        if let Some(release) = self.release.take() { let _ = release.send(()); }
    }
}

struct Fixture {
    dir: PathBuf,
    node: Arc<Runtime>,
    admin: LocalIdentity,
    backup: LocalIdentity,
    state: NetworkState,
    probes: TcpListener,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("client-concurrency-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let dir = dir.canonicalize()?;
        let probes = TcpListener::bind("127.0.0.1:0").await?;
        let bin = dir.join("fixture-pi");
        // The real provider-probe process waits for a byte from the test. Its control socket
        // is also a cancellation witness: EOF means the child actually exited.
        std::fs::write(&bin, r#"#!/usr/bin/env python3
import json, os, socket, sys
assert sys.argv[1:] == ['--mode', 'rpc', '--no-session', '--no-tools', '--no-extensions']
assert json.loads(sys.stdin.readline()) == {'id':'provider-probe','type':'get_available_models'}
with socket.create_connection(('127.0.0.1', int(os.environ['CLIENT_FIXTURE_PORT']))) as control:
    control.sendall(b'R')
    assert control.recv(1) == b'G'
    print(json.dumps({'id':'provider-probe','type':'response','success':True,'data':{'models':[{'provider':'fixture','id':'held-model','name':'Held model','reasoning':False}]}}), flush=True)
"#)?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700))?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.providers.execution = [Provider::Pi].into();
        configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
        configuration.bins.insert(Provider::Pi, bin);
        configuration.provider_env.apply(Provider::Pi, BTreeMap::from([(
            "CLIENT_FIXTURE_PORT".into(), Some(probes.local_addr()?.port().to_string()),
        )]))?;
        initialize_node(&dir, &configuration, &LocalIdentity::generate())?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let admin = LocalIdentity::generate();
        let backup = LocalIdentity::generate();
        let mut state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&admin, "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        state.append(GovernanceEvent::grant(
            &admin, &state.project()?, Member::client(&backup, "backup", true)?,
        )?)?;
        node.install_membership(state.clone()).await?;
        Ok(Self { dir, node, admin, backup, state, probes })
    }

    async fn connect(
        &self,
        identity: &LocalIdentity,
        cookie: Option<String>,
    ) -> Result<(FrameClient, tokio::task::JoinHandle<Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let (client, server) = tokio::join!(
            TcpStream::connect(listener.local_addr()?), listener.accept(),
        );
        let client = client?;
        client.set_nodelay(true)?;
        let client = WebSocketStream::from_raw_socket(
            MaybeTlsStream::Plain(client), Role::Client, None,
        ).await;
        let server = server?.0;
        server.set_nodelay(true)?;
        let server = WebSocketStream::from_raw_socket(
            MaybeTlsStream::Plain(server), Role::Server, None,
        ).await;
        let serving = tokio::spawn(serve_session_with_heartbeat(
            self.node.clone(), ClientSocket::Client(Box::new(server)), cookie,
            Duration::from_millis(100),
        ));
        let mut client = FrameClient::from_socket(
            ClientSocket::Client(Box::new(client)), identity, &self.node.identity.device_id()?,
        ).await?;
        client.call("authenticate", json!({})).await?;
        Ok((client, serving))
    }

    async fn probe_started(&self) -> Result<TcpStream> {
        let (mut control, _) = tokio::time::timeout(DEADLINE, self.probes.accept()).await??;
        let mut byte = [0];
        tokio::time::timeout(DEADLINE, control.read_exact(&mut byte)).await??;
        ensure!(byte == [b'R'], "fixture did not reach the held probe");
        Ok(control)
    }

    async fn cleanup(self) -> Result<()> {
        self.node.shutdown().await;
        drop(self.node);
        std::fs::remove_dir_all(self.dir)?;
        Ok(())
    }

    async fn accepted_work(&self) -> Result<()> {
        use crate::model::{EventBody, FirstOwner, ResolvedModel};
        use crate::work::{RequestedWorkKind, WorkRequest};
        use crate::workspace::{InputIntent, InputRequest, InputRoute, InputTarget};
        let network = &self.state.genesis.network_id;
        let node_id = self.node.identity.device_id()?;
        let owner = self.admin.attest(network, "scope", EventBody::ConversationOwner {
            scheduler_node: node_id.clone(), epoch: 1, attestation: None, cause: None,
        })?;
        let bot = self.admin.attest(network, "scope", EventBody::BotCreated {
            bot_id: "fixture-bot".into(), name: "Fixture bot".into(),
            instructions: "Use only controlled fixtures".into(), project: None,
            attestation: None, cause: None,
        })?;
        let input = InputRequest {
            message_id: "fixture-message".into(),
            route: InputRoute {
                target: InputTarget::BotMain { bot_id: "fixture-bot".into() },
                intent: InputIntent::Continue, sequence: 1, expected_revision: None,
            },
            text: "Fixture input".into(), attachments: vec![], report_results: true,
        };
        let signed_input = self.admin.attest(network, "scope", EventBody::InputPosted {
            message_id: input.message_id.clone(), target: input.route.target.clone(),
            intent: input.route.intent, sequence: input.route.sequence,
            expected_revision: input.route.expected_revision, text: input.text.clone(),
            attachments: input.attachments.clone(), report_results: Some(input.report_results),
            attestation: None,
        })?;
        let network = network.clone();
        let identity = self.node.identity.clone();
        let cwd = self.dir.display().to_string();
        self.node.store.call("fixture_accepted_work", move |store| {
        store.create_conversation(&identity, &network, "scope", "Fixture work",
            FirstOwner { scheduler_node: node_id.clone(), attestation: owner.attestation().cloned().context("owner attestation")? },
            crate::runtime::now_ms())?;
        store.append_event(&identity, &network, "scope", bot, vec![], crate::runtime::now_ms())?;
        store.post_workspace_input(&identity, &network, input,
            signed_input.attestation().cloned().context("fixture input signature")?,
            crate::runtime::now_ms())?;
        store.accept_work(&identity, &network, WorkRequest {
            work_id: fixture_work_id(), conversation: "scope".into(), trigger: "message:fixture-message".into(),
            target: node_id.clone(), dispatcher: node_id, call_ref: "fixture-call".into(),
            kind: RequestedWorkKind::Agent(Some(Provider::Pi)), model: None,
            cwd, title: "Fixture work".into(), prompt: "Never execute".into(),
            continues: None, parent: None, thread: None,
        }, ResolvedModel { provider: Provider::Pi, model: None }, crate::runtime::now_ms())?;
        Ok(())
        }).await
    }
}

async fn send(client: &mut FrameClient, id: &str, method: &str, params: Value) -> Result<()> {
    send_value(&mut client.socket, &mut client.channel, &Request {
        id: id.into(), method: method.into(), params,
    }).await
}

async fn receive(client: &mut FrameClient) -> Result<Value> {
    let bytes = tokio::time::timeout(DEADLINE,
        receive_value(&mut client.socket, &mut client.channel),
    ).await.context("response was blocked by the held probe")??;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn response(client: &mut FrameClient, id: &str) -> Result<Value> {
    loop {
        let value = receive(client).await?;
        if value["type"] == "changed" { continue; }
        ensure!(value["id"] == id, "unexpected response before {id}: {value}");
        return Ok(value);
    }
}

async fn probe_exited(control: &mut TcpStream) -> Result<()> {
    let mut byte = [0];
    assert_eq!(tokio::time::timeout(DEADLINE, control.read(&mut byte)).await??, 0,
        "the disconnected session left its probe process alive");
    Ok(())
}

async fn close(client: &mut FrameClient, serving: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    client.socket.send(Message::Close(None)).await?;
    let _ = tokio::time::timeout(DEADLINE, serving).await??;
    Ok(())
}

async fn ping_and_heartbeat(client: &mut FrameClient) -> Result<()> {
    client.socket.send(Message::Ping(b"writer-held".to_vec().into())).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            match client.socket.next().await.context("channel closed")?? {
                Message::Pong(bytes) if bytes.as_ref() == b"writer-held" => break,
                Message::Ping(_) => {},
                frame => bail!("unexpected frame before the held-writer pong: {frame:?}"),
            }
        }
        loop {
            match client.socket.next().await.context("channel closed")?? {
                Message::Ping(_) => return Ok::<_, anyhow::Error>(()),
                Message::Pong(_) => {},
                frame => bail!("unexpected frame before the held-writer heartbeat: {frame:?}"),
            }
        }
    }).await??;
    Ok(())
}

#[tokio::test]
async fn encrypted_workspace_overview_and_history_read_committed_rows_while_writer_is_held() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.accepted_work().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    let writer = HeldWriter::new(&fixture.node).await?;
    let result = async {
        for method in ["conversations", "open_works", "workspace_overview", "workspace_state", "workspace_history", "notifications"] {
            send(&mut client, method, "rpc", json!({"method":method})).await?;
        }
        send(&mut client, "conversation_history", "rpc", json!({
            "method":"conversation_history", "conversation_id":"scope", "section":"messages", "limit":1,
        })).await?;
        send(&mut client, "missing-display-scope", "rpc", json!({
            "method":"workspace_history", "conversation_id":"missing",
        })).await?;
        let mut replies = BTreeMap::new();
        while replies.len() < 8 {
            let reply = receive(&mut client).await?;
            let id = reply["id"].as_str().context("display response ID")?.to_owned();
            assert!(replies.insert(id, reply).is_none(), "duplicate display reply");
        }
        for method in ["conversations", "open_works", "workspace_overview", "workspace_state", "workspace_history", "notifications"] {
            assert!(replies[method]["error"].is_null(), "{}", replies[method]);
        }
        assert_eq!(replies["conversations"]["result"]["conversations"][0]["title"], "Fixture work");
        let overview = &replies["workspace_overview"]["result"];
        assert_eq!(overview["network_epoch"], fixture.state.project()?.epoch);
        assert_eq!(overview["coverage"]["source"], "workspace_overview");
        assert_eq!(overview["consistency"], "local_snapshot");
        assert_eq!(replies["workspace_state"]["result"]["tasks"], json!([]));
        assert_eq!(replies["workspace_history"]["result"]["events"], json!([]));
        assert_eq!(replies["workspace_history"]["result"]["coverage"]["scope"], "workspace");
        assert_eq!(replies["notifications"]["result"]["notifications"], json!([]));
        let history = &replies["conversation_history"]["result"];
        assert!(replies["conversation_history"]["error"].is_null(), "{}", replies["conversation_history"]);
        assert_eq!(history["conversation_id"], "scope");
        assert_eq!(history["section"], "messages");
        assert!(history["history_authority"].as_str().is_some_and(|value| !value.is_empty()));
        assert_eq!(history["items"].as_array().context("history messages")?.len(), 1);
        assert_eq!(history["items"][0]["message_id"], "fixture-message");
        assert_eq!(history["items"][0]["text"], "Fixture input");
        assert_eq!(history["complete"], true);
        assert!(history["next_cursor"].is_null());
        assert!(replies["missing-display-scope"]["error"].as_str()
            .is_some_and(|error| error.contains("conversation not found")));
        ping_and_heartbeat(&mut client).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    writer.finish().await?;
    close(&mut client, serving).await?;
    fixture.cleanup().await?;
    result
}

#[tokio::test]
async fn a_held_writer_does_not_block_encrypted_control_intake_pong_heartbeat_or_completed_result() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.accepted_work().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    client.call("subscribe", json!({})).await?;
    send(&mut client, "complete-under-writer", "provider_probe", json!({"provider":"pi"})).await?;
    let mut probe = fixture.probe_started().await?;
    let writer = HeldWriter::new(&fixture.node).await?;
    let result = async {
        for index in 0..CONTROL_CALLS {
            send(&mut client, &format!("stop-under-writer-{index}"), "rpc", json!({
                "method":"interrupt_work", "conversation_id":"scope", "work_id":fixture_work_id(),
            })).await?;
        }
        send(&mut client, "control-intake-barrier", "rpc", json!({
            "method":"interrupt_work", "conversation_id":"scope", "work_id":fixture_work_id(),
        })).await?;
        let overload = response(&mut client, "control-intake-barrier").await?;
        assert_eq!(overload["code"], "client_overloaded",
            "the encrypted stop frames must be admitted before the writer is released");
        send(&mut client, "read-under-writer", "rpc", json!({"method":"conversations"})).await?;
        let read = response(&mut client, "read-under-writer").await?;
        assert!(read["error"].is_null(), "{read}");
        assert_eq!(read["result"]["conversations"][0]["title"], "Fixture work");
        fixture.node.publish_reread(&fixture.state.genesis.network_id);
        // Control authentication and subscribed push preparation still wait on the writer.
        ping_and_heartbeat(&mut client).await?;
        probe.write_all(b"G").await?;
        let completed = response(&mut client, "complete-under-writer").await?;
        assert!(completed["error"].is_null(), "{completed}");
        assert_eq!(completed["result"]["probe"]["catalog"]["models"][0]["id"], "fixture/held-model");
        Ok::<_, anyhow::Error>(())
    }.await;
    writer.finish().await?;
    if result.is_ok() {
        let mut returned = BTreeSet::new();
        while returned.len() < CONTROL_CALLS {
            let value = receive(&mut client).await?;
            if value["type"] == "changed" { continue; }
            assert!(value["error"].is_null(), "{value}");
            returned.insert(value["id"].as_str().context("response ID")?.to_owned());
        }
        for index in 0..CONTROL_CALLS { assert!(returned.contains(&format!("stop-under-writer-{index}"))); }
        assert_eq!(fixture.node.store.call("fixture_stopped_under_writer", |store| store.stop_requests()).await?[0].work_id, fixture_work_id());
    }
    close(&mut client, serving).await?;
    fixture.cleanup().await?;
    result
}

#[tokio::test]
async fn a_cancelled_membership_installer_fences_socket_results_while_the_next_writer_is_held() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    send(&mut client, "held-before-revoke", "provider_probe", json!({"provider":"pi"})).await?;
    let mut probe = fixture.probe_started().await?;
    let writer = HeldWriter::new(&fixture.node).await?;
    fixture.state.append(GovernanceEvent::revoke(&fixture.backup, &fixture.state.project()?,
        Revocation::new(&fixture.admin.device_id()?, None)?)?)?;
    let state = fixture.state.clone();
    // Poll through admission into the empty writer queue, then cancel the installer. The
    // accepted commit must publish its authority fence without a caller continuation.
    let mut installer = Box::pin(fixture.node.store.call("fixture_cancelled_installer", move |store| {
        store.install_state(state)?;
        Ok(())
    }));
    assert!(futures_util::poll!(installer.as_mut()).is_pending());
    drop(installer);
    writer.finish().await?;
    let next_writer = HeldWriter::new(&fixture.node).await?;
    let closed = tokio::time::timeout(DEADLINE, serving).await??;
    assert!(closed.is_err(), "revocation must fence the pending response while a writer remains occupied");
    probe_exited(&mut probe).await?;
    assert!(receive(&mut client).await.is_err(), "a revoked probe result reached the socket");
    next_writer.finish().await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn a_committed_revocation_fences_results_before_the_writer_closure_returns() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    send(&mut client, "probe-during-revoke", "provider_probe", json!({"provider":"pi"})).await?;
    let mut probe = fixture.probe_started().await?;
    fixture.state.append(GovernanceEvent::revoke(&fixture.backup, &fixture.state.project()?,
        Revocation::new(&fixture.admin.device_id()?, None)?)?)?;
    let state = fixture.state.clone();
    // install_state commits before this barrier. Updating authority must withhold old data,
    // while keeping the transport alive until the definitive committed view is published.
    let writer = HeldWriter::after(&fixture.node, move |store| {
        store.install_state(state)?;
        Ok(())
    }).await?;
    probe.write_all(b"G").await?;
    probe_exited(&mut probe).await?;
    ping_and_heartbeat(&mut client).await?;
    assert!(!serving.is_finished(), "an updating cache alone does not establish revocation");
    writer.finish().await?;
    let closed = tokio::time::timeout(DEADLINE, serving).await??;
    assert!(closed.is_err(), "the published revocation must close the old connection");
    assert!(receive(&mut client).await.is_err(), "the physically revoked device received its old result");
    fixture.cleanup().await
}

#[tokio::test]
async fn unrelated_member_grants_pause_sensitive_results_then_resume_the_same_connection() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    client.call("subscribe", json!({})).await?;
    send(&mut client, "probe-during-grant", "provider_probe", json!({"provider":"pi"})).await?;
    let mut probe = fixture.probe_started().await?;
    fixture.state.append(GovernanceEvent::grant(&fixture.admin, &fixture.state.project()?,
        Member::client(&LocalIdentity::generate(), "unrelated", false)?)?)?;
    let state = fixture.state.clone();
    let writer = HeldWriter::after(&fixture.node, move |store| {
        store.install_state(state)?;
        Ok(())
    }).await?;
    probe.write_all(b"G").await?;
    probe_exited(&mut probe).await?;
    fixture.node.publish_reread(&fixture.state.genesis.network_id);
    ping_and_heartbeat(&mut client).await?;
    assert!(!serving.is_finished(), "an unrelated grant cannot reset an authorized connection");
    writer.finish().await?;
    let completed = response(&mut client, "probe-during-grant").await?;
    assert!(completed["error"].is_null(), "{completed}");
    assert_eq!(completed["result"]["probe"]["catalog"]["models"][0]["id"], "fixture/held-model");
    assert_eq!(client.call("authenticate", json!({})).await?["principal"]["device_id"], fixture.admin.device_id()?);
    close(&mut client, serving).await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn held_process_does_not_block_reads_stop_push_heartbeat_or_overload_responses() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.accepted_work().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    let result = async {
        client.call("subscribe", json!({})).await?;
        send(&mut client, "slow-0", "provider_probe", json!({"provider":"pi"})).await?;
        let mut control = fixture.probe_started().await?;
        // Fill the long lane while the provider's own lock keeps three more probes waiting.
        for index in 1..LONG_CALLS {
            send(&mut client, &format!("slow-{index}"), "provider_probe", json!({"provider":"pi"})).await?;
        }
        send(&mut client, "overflow", "provider_probe", json!({"provider":"pi"})).await?;
        let overloaded = receive(&mut client).await?;
        assert_eq!(overloaded["id"], "overflow");
        assert_eq!(overloaded["code"], "client_overloaded");

        send(&mut client, "fast", "rpc", json!({"method":"conversations"})).await?;
        let fast = receive(&mut client).await?;
        assert_eq!(fast["id"], "fast");
        assert_eq!(fast["result"]["conversations"][0]["title"], "Fixture work");

        send(&mut client, "stop", "rpc", json!({
            "method":"interrupt_work", "conversation_id":"scope", "work_id":fixture_work_id(),
        })).await?;
        let stopped = response(&mut client, "stop").await?;
        assert!(stopped["error"].is_null(), "{stopped}");
        assert_eq!(stopped["result"]["interrupt_requested"], true);
        assert_eq!(stopped["result"]["waiting_for_start"], true);
        assert_eq!(fixture.node.store.call("fixture_stops", |store| store.stop_requests()).await?[0].work_id, fixture_work_id());

        fixture.node.events.send(crate::runtime::Update {
            network_id: fixture.state.genesis.network_id.clone(), payload: json!({"type":"fixture-marker"}),
        }).map_err(|_| anyhow!("subscription closed before the fixture push"))?;
        loop {
            let pushed = receive(&mut client).await?;
            assert_eq!(pushed["type"], "changed", "probe completed before release: {pushed}");
            if pushed["payload"]["type"] == "fixture-marker" { break; }
        }

        client.socket.send(Message::Ping(b"control-marker".to_vec().into())).await?;
        tokio::time::timeout(DEADLINE, async {
            loop {
                match client.socket.next().await.context("channel closed")?? {
                    Message::Pong(bytes) if bytes.as_ref() == b"control-marker" => break,
                    Message::Ping(_) => {},
                    frame => bail!("unexpected frame while probe was held: {frame:?}"),
                }
            }
            // A heartbeat after the echoed marker cannot be an old buffered heartbeat.
            loop {
                match client.socket.next().await.context("channel closed")?? {
                    Message::Ping(_) => return Ok::<_, anyhow::Error>(()),
                    Message::Pong(_) => {},
                    frame => bail!("unexpected frame before heartbeat: {frame:?}"),
                }
            }
        }).await??;

        let mut replies = BTreeSet::new();
        for index in 0..LONG_CALLS {
            control.write_all(b"G").await?;
            let reply = receive(&mut client).await?;
            assert!(reply["error"].is_null(), "{reply}");
            assert_eq!(reply["result"]["probe"]["catalog"]["models"][0]["id"], "fixture/held-model");
            assert!(replies.insert(reply["id"].as_str().context("response ID")?.to_owned()));
            if index + 1 < LONG_CALLS { control = fixture.probe_started().await?; }
        }
        assert_eq!(replies, (0..LONG_CALLS).map(|index| format!("slow-{index}")).collect::<BTreeSet<_>>());
        // A new request can use capacity released by completed probes.
        send(&mut client, "after", "info", json!({})).await?;
        assert_eq!(receive(&mut client).await?["id"], "after");
        Ok::<_, anyhow::Error>(())
    }.await;
    close(&mut client, serving).await?;
    fixture.cleanup().await?;
    result
}

#[tokio::test]
async fn disconnect_aborts_the_probe_process_and_all_capacity_waiters() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    send(&mut client, "held", "provider_probe", json!({"provider":"pi"})).await?;
    let mut control = fixture.probe_started().await?;
    for index in 1..LONG_CALLS {
        send(&mut client, &format!("waiting-{index}"), "provider_probe", json!({"provider":"pi"})).await?;
    }
    // A response after these frames proves the transport admitted the queued probes.
    assert_eq!(client.call("info", json!({})).await?["initialized"], true);
    close(&mut client, serving).await?;
    probe_exited(&mut control).await?;
    let mut next_probe = Box::pin(fixture.probes.accept());
    assert!(futures_util::poll!(next_probe.as_mut()).is_pending());
    drop(next_probe);
    fixture.cleanup().await
}

#[tokio::test]
async fn duplicate_request_ids_close_the_channel_without_executing_a_second_probe() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    send(&mut client, "same", "provider_probe", json!({"provider":"pi"})).await?;
    let mut control = fixture.probe_started().await?;
    send(&mut client, "same", "provider_probe", json!({"provider":"pi"})).await?;
    let error = tokio::time::timeout(DEADLINE, serving).await??.unwrap_err();
    assert!(error.to_string().contains("duplicate request ID"), "{error}");
    probe_exited(&mut control).await?;
    let mut next_probe = Box::pin(fixture.probes.accept());
    assert!(futures_util::poll!(next_probe.as_mut()).is_pending());
    drop(next_probe);
    fixture.cleanup().await
}

#[tokio::test]
async fn a_completed_write_id_cannot_execute_another_side_effect() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    send(&mut client, "write-once", "local_access", json!({"enabled":true})).await?;
    assert_eq!(response(&mut client, "write-once").await?["result"]["enabled"], true);
    send(&mut client, "write-once", "local_access", json!({"enabled":false})).await?;
    let error = tokio::time::timeout(DEADLINE, serving).await??.unwrap_err();
    assert!(error.to_string().contains("duplicate request ID"), "{error}");
    assert!(fixture.node.local_access.enabled(), "the duplicate write must never run");
    fixture.cleanup().await
}

#[tokio::test]
async fn ordered_queue_is_bounded_and_does_not_consume_read_or_stop_capacity() -> Result<()> {
    let fixture = Fixture::new().await?;
    let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
    let save = fixture.node.config_save.lock().await;
    send(&mut client, "ordered-0", "configure_node", json!({"roots":[],"manager":null})).await?;
    // This independent response establishes that ordered-0 was admitted before the queue fills.
    assert_eq!(client.call("info", json!({})).await?["initialized"], true);
    for index in 1..=ORDERED_QUEUE {
        send(&mut client, &format!("ordered-{index}"), "configure_node", json!({"roots":[],"manager":null})).await?;
    }
    send(&mut client, "ordered-overflow", "configure_node", json!({"roots":[],"manager":null})).await?;
    let overflow = response(&mut client, "ordered-overflow").await?;
    assert_eq!(overflow["code"], "client_overloaded");
    send(&mut client, "stop-during-ordered", "rpc", json!({
        "method":"interrupt_work", "conversation_id":"missing", "work_id":"missing",
    })).await?;
    let stopped = response(&mut client, "stop-during-ordered").await?;
    assert!(stopped["error"].as_str().is_some_and(|error| error.contains("conversation not found")), "{stopped}");
    drop(save);
    for index in 0..=ORDERED_QUEUE {
        let reply = response(&mut client, &format!("ordered-{index}")).await?;
        assert!(reply["error"].is_null(), "{reply}");
    }
    close(&mut client, serving).await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn revocation_and_permission_changes_abort_unsubscribed_inflight_work() -> Result<()> {
    for revoke in [true, false] {
        let mut fixture = Fixture::new().await?;
        let (mut client, serving) = fixture.connect(&fixture.admin, None).await?;
        send(&mut client, "held", "provider_probe", json!({"provider":"pi"})).await?;
        let mut control = fixture.probe_started().await?;
        let event = if revoke {
            GovernanceEvent::revoke(&fixture.backup, &fixture.state.project()?,
                Revocation::new(&fixture.admin.device_id()?, None)?)?
        } else {
            GovernanceEvent::grant(&fixture.backup, &fixture.state.project()?,
                Member::client(&fixture.admin, "admin", false)?)?
        };
        fixture.state.append(event)?;
        fixture.node.install_membership(fixture.state.clone()).await?;
        let error = tokio::time::timeout(DEADLINE, serving).await??.unwrap_err();
        assert!(error.to_string().contains(if revoke { "not a member" } else { "permission changed" }), "{error}");
        probe_exited(&mut control).await?;
        // No encrypted probe result can be delivered after the authority loss.
        assert!(receive(&mut client).await.is_err());
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn local_cookie_revocation_aborts_the_held_process() -> Result<()> {
    let fixture = Fixture::new().await?;
    let browser = LocalIdentity::generate();
    let node_id = fixture.node.identity.device_id()?;
    let network = &fixture.state.genesis.network_id;
    fixture.node.local_access.publish_listener("http://127.0.0.1:0".into(), node_id.clone())?;
    let token = fixture.node.local_access.issue_from_local_setup(&node_id, network)?;
    let cookie = fixture.node.local_access.exchange(&token, &node_id, network, &browser.device_id()?)?;
    let (mut client, serving) = fixture.connect(&browser, Some(cookie)).await?;
    send(&mut client, "held", "provider_probe", json!({"provider":"pi"})).await?;
    let mut control = fixture.probe_started().await?;
    let writer = HeldWriter::new(&fixture.node).await?;
    fixture.node.local_access.set_enabled(false)?;
    let error = tokio::time::timeout(DEADLINE, serving).await??.unwrap_err();
    assert!(error.to_string().contains("local browser access was revoked"), "{error}");
    probe_exited(&mut control).await?;
    assert!(receive(&mut client).await.is_err());
    writer.finish().await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn a_completed_display_result_is_rechecked_even_before_membership_watch_is_polled() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let mut session = Snapshot { device_id: fixture.admin.device_id()?, cookie: None, operator: false }.session();
    let authority = Authority::new(&fixture.node, &session, fixture.node.local_access.subscribe())?;
    let mut calls = Calls::default();
    calls.accept(&fixture.node, &session, &authority, Request {
        id: "completed".into(), method: "rpc".into(), params: json!({"method":"conversations"}),
    })?;
    let completed = tokio::time::timeout(DEADLINE, calls.next()).await??;
    let prepared = PreparedPush {
        authority: authority.principal(&fixture.node, &session)?,
        payload: json!({"type":"private-fixture-records"}),
    };
    fixture.state.append(GovernanceEvent::revoke(&fixture.backup, &fixture.state.project()?,
        Revocation::new(&fixture.admin.device_id()?, None)?)?)?;
    fixture.node.install_membership(fixture.state.clone()).await?;
    let writer = HeldWriter::new(&fixture.node).await?;
    assert!(calls.deliver(&fixture.node, &mut session, &authority, completed).is_err());
    assert!(Pushes::default().deliver(&fixture.node, &session, &authority, prepared).is_err());
    drop(calls);
    writer.finish().await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn a_completed_invite_does_not_release_its_result_after_the_manager_is_revoked() -> Result<()> {
    use vibeface_secure::join::{INVITE_DOMAIN, invite_hash};
    let mut fixture = Fixture::new().await?;
    let mut session = Snapshot { device_id: fixture.admin.device_id()?, cookie: None, operator: false }.session();
    let authority = Authority::new(&fixture.node, &session, fixture.node.local_access.subscribe())?;
    let mut record = InviteRecord {
        network_id: fixture.state.genesis.network_id.clone(),
        invite_hash: invite_hash(&[3; 32]), issuer: fixture.admin.device_id()?,
        expires_at_ms: crate::runtime::now_ms() + 60_000, signature: String::new(),
    };
    record.signature = fixture.admin.sign_payload(INVITE_DOMAIN, &record.signing_payload()?)?;
    let mut calls = Calls::default();
    calls.accept(&fixture.node, &session, &authority, Request {
        id: "manager-invite".into(), method: "invite".into(), params: json!({"record":record}),
    })?;
    calls.start_ordered(&fixture.node, &session, &authority);
    let completed = tokio::time::timeout(DEADLINE, calls.next()).await??;
    assert!(completed.result.as_ref().expect("signed manager invite succeeds")["join_id"].is_string());
    fixture.state.append(GovernanceEvent::revoke(&fixture.backup, &fixture.state.project()?,
        Revocation::new(&fixture.admin.device_id()?, None)?)?)?;
    fixture.node.install_membership(fixture.state.clone()).await?;
    let writer = HeldWriter::new(&fixture.node).await?;
    assert!(calls.deliver(&fixture.node, &mut session, &authority, completed).is_err(),
        "the manager's completed invite must stay private after revocation");
    drop(calls);
    writer.finish().await?;
    fixture.cleanup().await
}
