use crate::{
    config::{self, Config},
    crypto::{self, LocalIdentity},
    identity::{AuthorizationView, GovernanceAction, IdentityKind, NetworkState},
    maintenance::{Busy, Gate},
    model::*,
    network::{IncomingSocket, NetworkHub, Service},
    peer::{AuthorizationDenied, Inbound, PeerHub, RequestFailure},
    store::Store,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};

mod agent_tools;
mod blob_transfer;
mod bots;
mod chat_controls;
mod followup;
mod manager_control;
#[cfg(test)]
mod manager_control_tests;
pub(crate) mod native_observation;
mod observation;
mod schedule_delivery;
mod sync;
pub use manager_control::ManagerAccess;
pub(crate) use observation::ObservationGuard;
use sync::{HistoryPage, HistoryReply, ReadClass, SyncCoordinator};

fn node_display_names(bundle: &NetworkState) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for member in bundle.genesis.initial_members.iter().chain(
        bundle
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.members.as_slice())
            .unwrap_or(&[]),
    ) {
        if member.kind == IdentityKind::Node {
            names.insert(member.subject_id.clone(), member.name.clone());
        }
    }
    for event in &bundle.events {
        if let GovernanceAction::Grant { member } = &event.action
            && member.kind == IdentityKind::Node
        {
            names.insert(member.subject_id.clone(), member.name.clone());
        }
    }
    names
}

#[cfg(test)]
mod node_display_names_tests {
    use super::node_display_names;
    use crate::crypto::LocalIdentity;
    use crate::identity::{Genesis, GovernanceEvent, Member, NetworkState, NodeRole};
    use anyhow::Result;

    #[test]
    fn revoked_node_keeps_last_granted_name() -> Result<()> {
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let node = LocalIdentity::generate();
        let mut state = NetworkState::new(Genesis::create(
            &root,
            vec![Member::client(&admin, "admin", true)?],
        )?)?;
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::node(
                &node,
                "Mac Studio",
                vec![NodeRole::Replica, NodeRole::Scheduler],
            )?,
        )?)?;
        state.append(GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            &node.peer_id()?,
        )?)?;
        let names = node_display_names(&state);
        assert_eq!(
            names.get(&node.peer_id()?).map(String::as_str),
            Some("Mac Studio")
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct Access {
    pub network_id: String,
    pub human: bool,
    pub conversation_id: Option<String>,
    pub manager: Option<ManagerAccess>,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_owner(node: &Runtime) -> Result<LocalIdentity> {
        Ok(LocalIdentity {
            secret_key_hex: crypto::digest(
                format!("runtime-test-root:{}", node.identity.peer_id()?).as_bytes(),
            ),
        })
    }
    pub(super) fn fixture() -> Result<PathBuf> {
        fixture_under("")
    }
    pub(super) fn fixture_under(subdir: &str) -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(subdir)
            .join(format!("runtime-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":5}"#)?;
        let mut cfg = Config::local("127.0.0.1:4790".parse()?, vec![dir.canonicalize()?], false);
        cfg.codex_socket = Some(dir.join("unavailable.sock"));
        cfg.save(&dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        Ok(dir)
    }
    pub(super) async fn conversation(node: &Runtime) -> Result<String> {
        let owner = test_owner(node)?;
        let state = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        let view = node.install_membership(state).await?;
        node.append(
            &view.network_id,
            "chat",
            EventBody::ConversationCreated {
                title: "test".into(),
            },
        )
        .await?;
        Ok(view.network_id)
    }
    #[tokio::test]
    async fn m4_route_flight_survives_cancelled_waiter_and_never_sends_its_payload() -> Result<()> {
        let source_dir = fixture_under("network-reliability")?;
        let target_dir = fixture_under("network-reliability")?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let endpoint = format!("ws://{address}/api/network/ws");
        let mut target_config = Config::load(&target_dir)?;
        target_config.listen = address;
        target_config.public_url = format!("http://{address}");
        target_config.network.advertise_urls = vec![endpoint.clone()];
        target_config.save(&target_dir)?;
        let mut source_config = Config::load(&source_dir)?;
        source_config.network.bootstrap_urls = vec![endpoint];
        source_config.save(&source_dir)?;
        let source = Arc::new(Runtime::open(&source_dir).await?);
        let target = Arc::new(Runtime::open(&target_dir).await?);
        let owner = test_owner(&source)?;
        let membership = owner.sign_network_fixture(
            1,
            vec![
                source.identity.certificate(source.config.roles.clone())?,
                target.identity.certificate(target.config.roles.clone())?,
            ],
        )?;
        let network = membership.genesis.network_id.clone();
        source.install_membership(membership.clone()).await?;
        target.install_membership(membership).await?;
        let target_id = target.identity.peer_id()?;
        let mut tasks = tokio::task::JoinSet::new();
        let router = crate::api::router(target.clone());
        tasks.spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let server_gate = gate.clone();
        let stop = Arc::new(tokio::sync::Notify::new());
        let server_stop = stop.clone();
        let mut sockets = target
            .network_rx
            .lock()
            .await
            .take()
            .context("missing network receiver")?;
        let hub = target.hub.clone();
        let (entered, observed) = oneshot::channel();
        let (closed, closed_observed) = oneshot::channel();
        tasks.spawn(async move {
            let socket = sockets.recv().await.expect("target connection missing");
            assert_eq!(socket.service, Service::Peer);
            let _ = entered.send(());
            let _permit = server_gate.acquire().await.expect("handshake gate closed");
            tokio::select! {
                _ = hub.accept_transport(socket.socket) => {},
                _ = server_stop.notified() => {},
            }
            let mut additional = 0;
            while sockets.try_recv().is_ok() {
                additional += 1;
            }
            let _ = closed.send(additional);
        });
        let mut inbound = target
            .peer_rx
            .lock()
            .await
            .take()
            .context("missing peer receiver")?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        let responder = target.clone();
        tasks.spawn(async move {
            while let Some(message) = inbound.recv().await {
                requests.lock().await.push(message.payload.clone());
                let value = responder
                    .handle_peer(&message)
                    .await
                    .unwrap_or_else(|error| json!({"error":error.to_string()}));
                let _ = responder.hub.respond(&message, value).await;
            }
        });
        let cancelled_node = source.clone();
        let cancelled_network = network.clone();
        let cancelled_target = target_id.clone();
        let cancelled = tokio::spawn(async move {
            cancelled_node
                .raw_peer_request(
                    &cancelled_network,
                    &cancelled_target,
                    json!({"type":"cancelled_write_probe","operation_id":"never-send"}),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), observed).await??;
        let survivor_node = source.clone();
        let survivor_network = network.clone();
        let survivor_target = target_id.clone();
        let survivor = tokio::spawn(async move {
            survivor_node
                .raw_peer_request(
                    &survivor_network,
                    &survivor_target,
                    json!({"type":"capabilities"}),
                )
                .await
        });
        let mut background = source
            .route_flight(&network, &target_id, "background", None, None)
            .await?;
        let flight_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let flights = source.route_flights.lock().await;
                assert_eq!(flights.len(), 1);
                let flight = &flights[&(network.clone(), target_id.clone())];
                if flight.state.receiver_count() >= 3 {
                    break flight.id.clone();
                }
                drop(flights);
                tokio::task::yield_now().await;
            }
        })
        .await?;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        assert!(seen.lock().await.is_empty());
        assert!(matches!(*background.borrow(), RouteFlightState::Connecting));
        gate.add_permits(1);
        loop {
            if matches!(*background.borrow(), RouteFlightState::Ready(_)) {
                break;
            }
            tokio::time::timeout(Duration::from_secs(2), background.changed()).await??;
        }
        let reply = tokio::time::timeout(Duration::from_secs(2), survivor).await???;
        assert_eq!(reply, target.capabilities());
        assert_eq!(*seen.lock().await, vec![json!({"type":"capabilities"})]);
        assert_eq!(
            source.route_flights.lock().await[&(network.clone(), target_id.clone())].id,
            flight_id
        );
        stop.notify_one();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), closed_observed).await??,
            0,
            "background and concurrent demand must share one actual peer data connection"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !source.route_flights.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(!source.hub.has_route(&network, &target_id).await);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop((source, target));
        std::fs::remove_dir_all(source_dir)?;
        std::fs::remove_dir_all(target_dir)?;
        eprintln!(
            "NET1_M4 one_actual_peer_data_connection=true background_and_demand_share_flight=true cancelled_waiter_did_not_cancel_owner=true cancelled_payload_never_sent=true survivor_capabilities_response=true flight_removed_after_disconnect=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn net1_bad_prefix_rotates_from_the_first_source_and_empty_blob_falls_back() -> Result<()>
    {
        use crate::store::blob_transfer::BlobTransfer;
        use axum::{
            Router,
            extract::{State, WebSocketUpgrade},
            routing::get,
        };
        type Source = (Arc<PeerHub>, Arc<tokio::sync::Notify>);
        async fn upgrade(
            State((hub, disconnect)): State<Source>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(crate::peer::MAX_PACKET_BYTES)
                .max_frame_size(crate::peer::MAX_PACKET_BYTES)
                .on_upgrade(|socket| async move {
                    tokio::select! { _ = hub.accept(socket) => {}, _ = disconnect.notified() => {} }
                })
        }
        let mut dirs = Vec::new();
        let mut nodes = Vec::new();
        for _ in 0..3 {
            let dir = fixture_under("network-reliability")?;
            nodes.push(Arc::new(Runtime::open(&dir).await?));
            dirs.push(dir);
        }
        let owner = test_owner(&nodes[0])?;
        let state = owner.sign_network_fixture(
            1,
            nodes
                .iter()
                .map(|node| node.identity.certificate(node.config.roles.clone()))
                .collect::<Result<Vec<_>>>()?,
        )?;
        let network = state.genesis.network_id.clone();
        for node in &nodes {
            node.install_membership(state.clone()).await?;
        }
        let bytes = Arc::new(vec![0x57_u8; 96 * 1024]);
        let blob = BlobRef {
            hash: crypto::digest(&bytes),
            size: bytes.len() as u64,
        };
        let empty = BlobRef {
            hash: crypto::digest(b""),
            size: 0,
        };
        let observed = Arc::new(Mutex::new(Vec::<(usize, String, u64)>::new()));
        let mut tasks = tokio::task::JoinSet::new();
        let mut disconnects = Vec::new();
        for index in 1..=2 {
            let source = nodes[index].clone();
            let disconnect = Arc::new(tokio::sync::Notify::new());
            disconnects.push(disconnect.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("ws://{}/peer", listener.local_addr()?);
            let router = Router::new()
                .route("/peer", get(upgrade))
                .with_state((source.hub.clone(), disconnect.clone()));
            tasks.spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            let hub = nodes[0].hub.clone();
            let scope = network.clone();
            tasks.spawn(async move {
                let _ = hub.connect(scope, url).await;
            });
            let mut inbound = source
                .peer_rx
                .lock()
                .await
                .take()
                .context("missing source test receiver")?;
            let observed = observed.clone();
            let bytes = bytes.clone();
            tasks.spawn(async move {
                while let Some(message) = inbound.recv().await {
                    let value = if message.payload["type"] == "blob" {
                        let hash = message.payload["hash"].as_str().unwrap().to_owned();
                        let offset = message.payload["offset"].as_u64().unwrap();
                        let limit = message.payload["limit"].as_u64().unwrap() as usize;
                        observed.lock().await.push((index,hash,offset));
                        if index == 1 {
                            if offset == 0 {
                                json!({"size":bytes.len(),"data":hex::encode(vec![0x11_u8;limit.min(bytes.len())])})
                            } else {
                                // Drop the authenticated WebSocket after serving a stable corrupt prefix.
                                disconnect.notify_one();
                                continue;
                            }
                        } else if message.payload["hash"] == crypto::digest(b"") {
                            json!({"size":0,"data":""})
                        } else {
                            let start = offset as usize;
                            json!({"size":bytes.len(),"data":hex::encode(&bytes[start..start.saturating_add(limit).min(bytes.len())])})
                        }
                    } else { source.handle_peer(&message).await.unwrap_or_else(|error| json!({"error":error.to_string()})) };
                    let _ = source.hub.respond(&message,value).await;
                }
            });
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while nodes[0].hub.online(&network).await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        let a = nodes[1].identity.peer_id()?;
        let b = nodes[2].identity.peer_id()?;
        let sources = vec![
            HistorySource {
                peer_id: a.clone(),
                batch: None,
                failed: false,
            },
            HistorySource {
                peer_id: b.clone(),
                batch: None,
                failed: false,
            },
        ];
        let error = nodes[0]
            .history_blob(
                &network,
                &sources,
                &blob,
                ReadClass::Proof,
                &mut blob_transfer::BlobQuantum::new(4),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("hash mismatch"));
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .is_none()
        );
        let expected = BlobTransfer::receiving(&blob, &b, 0);
        // Read through an independent connection: the next-source choice survives outside this call.
        let connection = rusqlite::Connection::open(dirs[0].join("node.sqlite3"))?;
        let raw: String = connection.query_row(
            "SELECT value FROM local_values WHERE network_id=? AND key=?",
            rusqlite::params![network, BlobTransfer::key(&blob)?],
            |row| row.get(0),
        )?;
        assert_eq!(serde_json::from_str::<BlobTransfer>(&raw)?, expected);
        let first = observed.lock().await.clone();
        assert_eq!(
            first
                .iter()
                .map(|(index, _, offset)| (*index, *offset))
                .collect::<Vec<_>>(),
            vec![(1, 0), (1, 65536), (2, 65536)]
        );
        let boundary = first.len();
        assert_eq!(
            nodes[0]
                .history_blob(
                    &network,
                    &sources,
                    &blob,
                    ReadClass::Proof,
                    &mut blob_transfer::BlobQuantum::new(4)
                )
                .await?,
            blob_transfer::BlobStep::Complete { changed: true }
        );
        assert_eq!(observed.lock().await[boundary], (2, blob.hash.clone(), 0));
        assert_eq!(
            nodes[0]
                .store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .unwrap(),
            *bytes
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .blob_transfer(&network, &blob)?
                .is_none()
        );
        // A is still a valid member, but the actual transport is gone. Empty objects must try B.
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .authorize_node(&network, &a, Some(NodeRole::Replica))
                .is_ok()
        );
        let unavailable = nodes[0]
            .read_peer(
                &network,
                &a,
                json!({"type":"blob","hash":empty.hash,"offset":0,"limit":1}),
            )
            .await
            .unwrap_err();
        println!("NET1_EMPTY_SOURCE_FAILURE {unavailable:#}");
        assert_eq!(
            nodes[0]
                .history_blob(
                    &network,
                    &sources,
                    &empty,
                    ReadClass::Proof,
                    &mut blob_transfer::BlobQuantum::new(4)
                )
                .await?,
            blob_transfer::BlobStep::Complete { changed: true }
        );
        assert_eq!(
            nodes[0]
                .store
                .lock()
                .await
                .get_blob(&network, &empty.hash)?,
            Some(Vec::new())
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .blob_transfer(&network, &empty)?
                .is_none()
        );
        assert!(
            observed
                .lock()
                .await
                .iter()
                .any(|(index, hash, offset)| *index == 2 && hash == &empty.hash && *offset == 0)
        );
        {
            let store = nodes[0].store.lock().await;
            for scope in store.scopes(&network)? {
                assert!(
                    store
                        .scheduler_jobs(&network, &scope, &nodes[0].identity.peer_id()?)?
                        .is_empty()
                );
            }
            assert!(store.scheduler_outbox_all(&network)?.is_empty());
        }
        drop(connection);
        for disconnect in disconnects {
            disconnect.notify_one();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !nodes[0].hub.online(&network).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(nodes);
        for dir in dirs {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn net1_pending_basis_rejects_before_scanning_history_and_ready_rechecks_content()
    -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let script = dir.join("fixture-manager.py");
        let verifier = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/verify-node.py")
            .canonicalize()?;
        config::private_write(&script,format!(
            "import os\nos.execv('/usr/bin/python3',['/usr/bin/python3',{},'--fixture-manager','--fixture-root',{}])\n",
            json!(verifier.to_string_lossy()),json!(dir.to_string_lossy()),
        ).as_bytes())?;
        let mut cfg = Config::load(&dir)?;
        cfg.listen = "127.0.0.1:0".parse()?;
        cfg.codex_bin = "/usr/bin/true".into();
        cfg.manager = Some(config::ManagerSettings {
            provider: "codex_sdk".into(),
            workspace: dir.canonicalize()?,
            model: "controlled-fixture".into(),
            cursor_sdk_bin: "unused".into(),
            node_bin: "/usr/bin/python3".into(),
            codex_sdk_script: script,
        });
        cfg.save(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let content = b"basis content must be verified";
        let blob = BlobRef {
            hash: crypto::digest(content),
            size: content.len() as u64,
        };
        let root = node.store.lock().await.heads(&network, "chat")?[0].clone();
        let pending = node.identity.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: node.identity.peer_id()?,
            sequence: 2,
            previous: Some(root.clone()),
            parents: vec![root],
            created_at_ms: 2,
            body: EventBody::MessagePosted {
                message_id: "pending-basis".into(),
                role: "user".into(),
                text: "needs the attachment".into(),
                scheduler_node: None,
            },
            blobs: vec![blob.clone()],
        })?;
        let sentinel = node.identity.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: node.identity.peer_id()?,
            sequence: 3,
            previous: Some(pending.hash.clone()),
            parents: vec![pending.hash.clone()],
            created_at_ms: 3,
            body: EventBody::MessagePosted {
                message_id: "later-sentinel".into(),
                role: "user".into(),
                text: "outside the requested basis ancestry".into(),
                scheduler_node: None,
            },
            blobs: vec![],
        })?;
        node.store
            .lock()
            .await
            .ingest_events(&network, &[pending.clone(), sentinel.clone()])?;
        let mut corrupt = sentinel.clone();
        corrupt.signature = "00".repeat(64);
        let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
        connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            rusqlite::params![serde_json::to_string(&corrupt)?, network, sentinel.hash],
        )?;
        assert!(
            node.store
                .lock()
                .await
                .context(&network, "chat", std::slice::from_ref(&pending.hash))
                .is_err()
        );
        let human = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: Some("chat".into()),
            manager: None,
        };
        let input = json!({"method":"message","conversation_id":"chat","message_id":"verified-sdk-work",
            "scheduler_node":node.identity.peer_id()?,"basis_heads":[pending.hash],"report_results":false,
            "text":json!({"phase1":true,"label":"net1-basis","host_command":"printf net1 >> pending-effect","host_cwd":dir.canonicalize()?}).to_string()});
        assert_eq!(
            node.rpc(human.clone(), input.clone())
                .await
                .unwrap_err()
                .to_string(),
            "context is incomplete; wait for synchronization"
        );
        assert!(
            node.store
                .lock()
                .await
                .scheduler_jobs(&network, "chat", &node.identity.peer_id()?)?
                .is_empty()
        );
        assert!(!dir.join("pending-effect").exists());
        connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            rusqlite::params![serde_json::to_string(&sentinel)?, network, sentinel.hash],
        )?;
        node.store.lock().await.put_blob(&network, &blob, content)?;
        let status: String = connection.query_row(
            "SELECT status FROM events WHERE network_id=? AND hash=?",
            rusqlite::params![network, pending.hash],
            |row| row.get(0),
        )?;
        assert_eq!(status, "ready");
        connection.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            rusqlite::params![vec![b'x'; content.len()], network, blob.hash],
        )?;
        assert_eq!(
            node.store
                .lock()
                .await
                .missing_dependencies(&network, std::slice::from_ref(&pending.hash))?
                .blobs,
            vec![blob.clone()]
        );
        assert_eq!(
            node.rpc(human.clone(), input.clone())
                .await
                .unwrap_err()
                .to_string(),
            "context is incomplete; wait for synchronization"
        );
        assert!(
            node.store
                .lock()
                .await
                .scheduler_jobs(&network, "chat", &node.identity.peer_id()?)?
                .is_empty()
        );
        assert!(!dir.join("pending-effect").exists());
        node.store.lock().await.put_blob(&network, &blob, content)?;
        let accepted = node.rpc(human, input).await?;
        let api_node = node.clone();
        let api = tokio::spawn(async move { crate::api::serve(api_node).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while node.tool_url.get().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let access = Access {
            network_id: network.clone(),
            human: false,
            conversation_id: Some("chat".into()),
            manager: None,
        };
        let trigger = accepted["trigger"]
            .as_str()
            .context("accepted trigger missing")?
            .to_owned();
        for _ in 0..2 {
            assert_eq!(
                crate::scheduler::run(node.clone(), access.clone(), "chat".into(), trigger.clone())
                    .await?["state"],
                "completed"
            );
        }
        assert_eq!(std::fs::read(dir.join("pending-effect"))?, b"net1");
        assert_eq!(
            std::fs::read_to_string(dir.join("manager-invocations.jsonl"))?
                .lines()
                .count(),
            1
        );
        api.abort();
        let _ = api.await;
        drop(connection);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn net1_history_batch_checks_exact_closure_and_wire_bytes() -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let root = node.store.lock().await.heads(&network, "chat")?[0].clone();
        let root = node.store.lock().await.event(&network, &root)?.unwrap();
        let mut event: Event = serde_json::from_str(&root.canonical_body)?;
        event.sequence = 2;
        event.previous = Some(root.hash.clone());
        event.parents.clear();
        event.body = EventBody::MessagePosted {
            message_id: "previous-only".into(),
            role: "user".into(),
            text: "previous is an ancestry edge".into(),
            scheduler_node: None,
        };
        let parent = node.identity.sign_event(event.clone())?;
        event.sequence = 3;
        event.previous = Some(parent.hash.clone());
        let child = node.identity.sign_event(event.clone())?;
        let batch = json!({"version":1,"hash":child.hash,"events":[root,parent,child]});
        let store = node.store.lock().await;
        let verified = verify_history_batch(
            &store,
            &network,
            &child.hash,
            &batch,
            3,
            HISTORY_BATCH_BYTES,
        )?;
        assert_eq!(
            verified.iter().map(|event| &event.hash).collect::<Vec<_>>(),
            vec![&child.hash, &parent.hash, &root.hash]
        );
        let mut bad = batch.clone();
        bad["hash"] = json!(root.hash);
        assert!(
            verify_history_batch(&store, &network, &child.hash, &bad, 3, HISTORY_BATCH_BYTES)
                .is_err()
        );
        bad = batch.clone();
        bad["events"] = json!([root, parent]);
        assert_eq!(
            verify_history_batch(&store, &network, &child.hash, &bad, 3, HISTORY_BATCH_BYTES)
                .unwrap_err()
                .to_string(),
            "history batch root missing"
        );
        bad["events"] = json!([root, child]);
        assert_eq!(
            verify_history_batch(&store, &network, &child.hash, &bad, 3, HISTORY_BATCH_BYTES)
                .unwrap_err()
                .to_string(),
            "unrelated event in history batch"
        );
        bad["events"] = json!([child, child]);
        assert_eq!(
            verify_history_batch(&store, &network, &child.hash, &bad, 3, HISTORY_BATCH_BYTES)
                .unwrap_err()
                .to_string(),
            "duplicate event in history batch"
        );
        bad = batch.clone();
        bad["events"][0]["signature"] = json!("00".repeat(64));
        assert!(
            verify_history_batch(&store, &network, &child.hash, &bad, 3, HISTORY_BATCH_BYTES)
                .is_err()
        );
        event.network_id = "ab".repeat(32);
        let foreign = node.identity.sign_event(event.clone())?;
        bad["events"] = json!([child, foreign]);
        assert!(
            verify_history_batch(&store, &network, &child.hash, &bad, 3, HISTORY_BATCH_BYTES)
                .is_err()
        );
        assert!(
            verify_history_batch(
                &store,
                &network,
                &child.hash,
                &batch,
                2,
                HISTORY_BATCH_BYTES
            )
            .is_err()
        );
        let wire_bytes = serde_json::to_vec(&batch)?.len();
        assert!(
            verify_history_batch(&store, &network, &child.hash, &batch, 3, wire_bytes - 1).is_err()
        );
        drop(store);

        event.network_id = network.clone();
        event.scope_id = "other-scope".into();
        event.sequence = 1;
        event.previous = None;
        event.parents = vec![root.hash.clone()];
        let branch = node.identity.sign_event(event.clone())?;
        event.scope_id = "chat".into();
        event.sequence = 3;
        event.previous = Some(parent.hash.clone());
        event.parents = vec![root.hash.clone(), branch.hash.clone()];
        let diamond = node.identity.sign_event(event.clone())?;
        let batch = json!({"version":1,"hash":diamond.hash,"events":[root,parent,branch,diamond]});
        let store = node.store.lock().await;
        assert_eq!(
            verify_history_batch(
                &store,
                &network,
                &diamond.hash,
                &batch,
                4,
                HISTORY_BATCH_BYTES
            )?
            .len(),
            4
        );
        drop(store);

        event.scope_id = "large-event".into();
        event.sequence = 1;
        event.previous = None;
        event.parents.clear();
        event.body = EventBody::MessagePosted {
            message_id: "escaped".into(),
            role: "user".into(),
            text: "\"\\界".repeat(145_000),
            scheduler_node: None,
        };
        let large = node.identity.sign_event(event)?;
        assert!(large.canonical_body.len() > 1_000_000);
        node.store
            .lock()
            .await
            .ingest_events(&network, std::slice::from_ref(&large))?;
        let mut message = Inbound {
            network_id: network.clone(),
            origin: node.identity.peer_id()?,
            request_id: uuid::Uuid::new_v4().to_string(),
            broadcast: false,
            payload: json!({"type":"history_batch","hash":large.hash,"limit":128,"max_bytes":HISTORY_BATCH_BYTES}),
        };
        let response = node.handle_peer(&message).await?;
        let actual_bytes = serde_json::to_vec(&response)?.len();
        assert!(actual_bytes <= HISTORY_BATCH_BYTES);
        assert_eq!(response["events"].as_array().unwrap().len(), 1);
        assert!(actual_bytes > large.canonical_body.len());
        message.payload["max_bytes"] = json!(512);
        let response = node.handle_peer(&message).await?;
        assert_eq!(
            response,
            json!({"version":1,"hash":large.hash,"single_event":true})
        );
        message.payload["max_bytes"] =
            json!(serde_json::to_vec(&json!({"version":1,"hash":large.hash,"events":[]}))?.len());
        assert_eq!(
            node.handle_peer(&message).await.unwrap_err().to_string(),
            "history batch byte budget too small"
        );
        message.payload = json!({"type":"event","hash":large.hash});
        assert_eq!(node.handle_peer(&message).await?["event"], json!(large));

        let mut event: Event = serde_json::from_str(&root.canonical_body)?;
        event.scope_id = "wide".into();
        event.parents = (0..1024)
            .map(|index| crypto::digest(format!("missing-{index}").as_bytes()))
            .collect();
        let wide = node.identity.sign_event(event)?;
        node.store
            .lock()
            .await
            .ingest_events(&network, std::slice::from_ref(&wide))?;
        message.payload = json!({"type":"history_batch","hash":wide.hash,"limit":128,"max_bytes":HISTORY_BATCH_BYTES});
        let response = node.handle_peer(&message).await?;
        assert_eq!(response["events"].as_array().unwrap().len(), 1);
        assert_eq!(response["events"][0]["hash"], wide.hash);
        Ok(())
    }

    #[tokio::test]
    async fn net1_history_batch_advances_with_only_64_replay_slots() -> Result<()> {
        use axum::{
            Router,
            extract::{State, WebSocketUpgrade},
            routing::get,
        };
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        async fn upgrade(
            State(hub): State<Arc<PeerHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(crate::peer::MAX_PACKET_BYTES)
                .max_frame_size(crate::peer::MAX_PACKET_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }
        let receiver_dir = fixture_under("network-reliability")?;
        let source_dir = fixture_under("network-reliability")?;
        let backup_dir = fixture_under("network-reliability")?;
        let receiver = Arc::new(Runtime::open(&receiver_dir).await?);
        let source = Arc::new(Runtime::open(&source_dir).await?);
        let backup = Arc::new(Runtime::open(&backup_dir).await?);
        let owner = test_owner(&receiver)?;
        let state = owner.sign_network_fixture(
            1,
            vec![
                receiver
                    .identity
                    .certificate(receiver.config.roles.clone())?,
                source.identity.certificate(source.config.roles.clone())?,
                backup.identity.certificate(backup.config.roles.clone())?,
            ],
        )?;
        let network = state.genesis.network_id.clone();
        receiver.install_membership(state.clone()).await?;
        source.install_membership(state.clone()).await?;
        backup.install_membership(state).await?;
        let mut envelopes = Vec::<SignedEvent>::new();
        for index in 0..1025 {
            let previous = envelopes.last().map(|event| event.hash.clone());
            envelopes.push(source.identity.sign_event(Event {
                version: 1,
                network_id: network.clone(),
                scope_id: "chat".into(),
                author: source.identity.peer_id()?,
                sequence: index + 1,
                previous,
                parents: vec![],
                created_at_ms: index as i64,
                body: if index == 0 {
                    EventBody::ConversationCreated {
                        title: "bounded history".into(),
                    }
                } else {
                    EventBody::MessagePosted {
                        message_id: format!("m{index}"),
                        role: "user".into(),
                        text: format!("message {index}"),
                        scheduler_node: None,
                    }
                },
                blobs: vec![],
            })?);
        }
        let mut unrelated: Event = serde_json::from_str(&envelopes[0].canonical_body)?;
        unrelated.scope_id = "unrelated".into();
        let unrelated = source.identity.sign_event(unrelated)?;
        for batch in envelopes.chunks(1024) {
            source.store.lock().await.ingest_events(&network, batch)?;
            backup.store.lock().await.ingest_events(&network, batch)?;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/peer", listener.local_addr()?);
        let mut tasks = tokio::task::JoinSet::new();
        let router = Router::new()
            .route("/peer", get(upgrade))
            .with_state(source.hub.clone());
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let hub = receiver.hub.clone();
        let scope = network.clone();
        tasks.spawn(async move {
            let _ = hub.connect(scope, url).await;
        });
        let malformed = Arc::new(AtomicUsize::new(1));
        let legacy = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let batch_requests = Arc::new(AtomicUsize::new(0));
        let single_requests = Arc::new(AtomicUsize::new(0));
        let mut inbound = source
            .peer_rx
            .lock()
            .await
            .take()
            .context("missing test receiver")?;
        let serving = source.clone();
        let extra = unrelated.clone();
        let (bad, old, count, batches, singles) = (
            malformed.clone(),
            legacy.clone(),
            requests.clone(),
            batch_requests.clone(),
            single_requests.clone(),
        );
        tasks.spawn(async move {
            while let Some(message) = inbound.recv().await {
                count.fetch_add(1, Ordering::SeqCst);
                let mut value = serving
                    .handle_peer(&message)
                    .await
                    .unwrap_or_else(|error| json!({"error":error.to_string()}));
                if message.payload["type"] == "capabilities" && old.load(Ordering::SeqCst) {
                    value.as_object_mut().unwrap().remove("history_batch");
                }
                if message.payload["type"] == "event" {
                    singles.fetch_add(1, Ordering::SeqCst);
                }
                if message.payload["type"] == "history_batch" {
                    batches.fetch_add(1, Ordering::SeqCst);
                    if bad.load(Ordering::SeqCst) == 1 {
                        let events = value["events"].as_array_mut().unwrap();
                        events.pop();
                        events.push(json!(extra));
                    } else if bad.load(Ordering::SeqCst) == 2 {
                        value = json!({"error":"x".repeat(8192)});
                    } else if bad.load(Ordering::SeqCst) == 3 {
                        value["events"] = json!(vec![json!({}); 1024]);
                    } else if bad.load(Ordering::SeqCst) == 4 {
                        let mut small = message.clone();
                        small.payload["max_bytes"] = json!(120);
                        value = serving.handle_peer(&small).await.unwrap();
                    }
                }
                let _ = serving.hub.respond(&message, value).await;
            }
        });
        let source_id = source.identity.peer_id()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !receiver.hub.online(&network).await.contains(&source_id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        receiver.hub.reserve_replay_entries(&network, 64).await?;
        source.hub.reserve_replay_entries(&network, 64).await?;
        let head = &envelopes.last().unwrap().hash;
        let bad = receiver
            .fetch_scope(&network, "chat", &source_id, std::slice::from_ref(head))
            .await?;
        assert_ne!(bad.state, SyncState::Complete);
        assert_eq!(bad.processed, 128);
        assert!(receiver.store.lock().await.event(&network, head)?.is_none());
        assert!(
            receiver
                .store
                .lock()
                .await
                .event(&network, &unrelated.hash)?
                .is_none()
        );
        assert_eq!(single_requests.load(Ordering::SeqCst), 0);
        malformed.store(2, Ordering::SeqCst);
        let mut sources = receiver
            .history_sources(&network, std::slice::from_ref(&source_id))
            .await?;
        let mut budget = HistoryBudget {
            events: 5,
            bytes: 8192,
        };
        assert!(
            receiver
                .history_events(&network, &mut sources, head, &mut budget, ReadClass::Proof)
                .await
                .is_err()
        );
        assert_eq!(budget.bytes, 0);
        assert_eq!(budget.events, 4);
        malformed.store(0, Ordering::SeqCst);
        let first = receiver
            .fetch_closure_batch(&network, "chat", &source_id, std::slice::from_ref(head), 1)
            .await?;
        assert_eq!(first.processed, 1);
        assert_eq!(first.state, SyncState::Partial);
        assert_eq!(first.missing.events, vec![envelopes[1023].hash.clone()]);
        let final_page = receiver
            .fetch_scope(&network, "chat", &source_id, std::slice::from_ref(head))
            .await?;
        assert_eq!(final_page.state, SyncState::Complete);
        assert_eq!(final_page.processed, 1024);
        assert_eq!(
            receiver
                .store
                .lock()
                .await
                .messages(&network, "chat")?
                .len(),
            1024
        );
        assert!(requests.load(Ordering::SeqCst) < 24);
        assert_eq!(batch_requests.load(Ordering::SeqCst), 11);
        assert_eq!(single_requests.load(Ordering::SeqCst), 0);
        legacy.store(true, Ordering::SeqCst);
        let before = batch_requests.load(Ordering::SeqCst);
        let mut sources = receiver
            .history_sources(&network, std::slice::from_ref(&source_id))
            .await?;
        let mut budget = HistoryBudget {
            events: 128,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        let old = receiver
            .history_events(&network, &mut sources, head, &mut budget, ReadClass::Proof)
            .await?;
        assert_eq!(old.events, vec![envelopes.last().unwrap().clone()]);
        drop(old);
        assert_eq!(batch_requests.load(Ordering::SeqCst), before);
        assert_eq!(single_requests.load(Ordering::SeqCst), 1);
        assert!(requests.load(Ordering::SeqCst) < 24);

        legacy.store(false, Ordering::SeqCst);
        malformed.store(4, Ordering::SeqCst);
        let mut sources = receiver
            .history_sources(&network, std::slice::from_ref(&source_id))
            .await?;
        let mut budget = HistoryBudget {
            events: 128,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        let single = receiver
            .history_events(&network, &mut sources, head, &mut budget, ReadClass::Proof)
            .await?;
        assert_eq!(single.events, vec![envelopes.last().unwrap().clone()]);
        drop(single);
        assert_eq!(single_requests.load(Ordering::SeqCst), 2);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/peer", listener.local_addr()?);
        let router = Router::new()
            .route("/peer", get(upgrade))
            .with_state(backup.hub.clone());
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let hub = receiver.hub.clone();
        let scope = network.clone();
        tasks.spawn(async move {
            let _ = hub.connect(scope, url).await;
        });
        let mut inbound = backup
            .peer_rx
            .lock()
            .await
            .take()
            .context("missing test backup receiver")?;
        let serving = backup.clone();
        tasks.spawn(async move {
            while let Some(message) = inbound.recv().await {
                let value = serving
                    .handle_peer(&message)
                    .await
                    .unwrap_or_else(|error| json!({"error":error.to_string()}));
                let _ = serving.hub.respond(&message, value).await;
            }
        });
        let backup_id = backup.identity.peer_id()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !receiver.hub.online(&network).await.contains(&backup_id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        malformed.store(3, Ordering::SeqCst);
        let mut sources = receiver
            .history_sources(&network, std::slice::from_ref(&source_id))
            .await?;
        let mut budget = HistoryBudget {
            events: 1024,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        let recovered = receiver
            .history_events(&network, &mut sources, head, &mut budget, ReadClass::Proof)
            .await?;
        assert_eq!(recovered.len(), 128);
        assert_eq!(recovered[0].hash, *head);
        assert_eq!(budget.events, 768);
        tasks.abort_all();
        Ok(())
    }

    #[tokio::test]
    async fn net1_continuation_survives_restart_and_frontier_rechecks_corruption() -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let root = node.store.lock().await.heads(&network, "chat")?[0].clone();
        let content = b"verified network evidence";
        let blob = BlobRef {
            hash: crypto::digest(content),
            size: content.len() as u64,
        };
        let parent = node.identity.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: node.identity.peer_id()?,
            sequence: 2,
            previous: Some(root.clone()),
            parents: vec![root],
            created_at_ms: 2,
            body: EventBody::MessagePosted {
                message_id: "parent".into(),
                role: "user".into(),
                text: "parent".into(),
                scheduler_node: None,
            },
            blobs: vec![blob.clone()],
        })?;
        let child = node.identity.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: node.identity.peer_id()?,
            sequence: 3,
            previous: Some(parent.hash.clone()),
            parents: vec![parent.hash.clone()],
            created_at_ms: 3,
            body: EventBody::MessagePosted {
                message_id: "child".into(),
                role: "user".into(),
                text: "child".into(),
                scheduler_node: None,
            },
            blobs: vec![],
        })?;
        node.store.lock().await.ingest_event(&network, &child)?;
        let local = node.identity.peer_id()?;
        let first = node
            .fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
            .await?;
        assert_eq!(first.state, SyncState::Unreachable);
        assert_eq!(first.missing.events, vec![parent.hash.clone()]);
        assert!(
            node.store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        node._process_lock.unlock()?;
        drop(node);
        let node = Arc::new(Runtime::open(&dir).await?);
        let resumed = node
            .fetch_closure_batch(
                &network,
                "chat",
                &local,
                std::slice::from_ref(&child.hash),
                0,
            )
            .await?;
        assert_eq!(resumed.missing.events, vec![parent.hash.clone()]);
        node.install_membership(test_owner(&node)?.sign_network_fixture(
            2,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?)
        .await?;
        let after_membership = node
            .fetch_closure_batch(
                &network,
                "chat",
                &local,
                std::slice::from_ref(&child.hash),
                0,
            )
            .await?;
        assert_eq!(after_membership.missing.events, vec![parent.hash.clone()]);
        node.store.lock().await.ingest_event(&network, &parent)?;
        node.store.lock().await.put_blob(&network, &blob, content)?;
        let complete = node
            .fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
            .await?;
        assert_eq!(complete.state, SyncState::Complete);
        assert_eq!(node.store.lock().await.messages(&network, "chat")?.len(), 2);
        let generation = node.store.lock().await.projection_generation(&network)?;
        let noop = node
            .fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
            .await?;
        assert_eq!(noop.state, SyncState::Complete);
        assert!(!noop.changed);
        assert_eq!(
            node.store.lock().await.projection_generation(&network)?,
            generation
        );
        node.append(
            &network,
            "unrelated",
            EventBody::ConversationCreated {
                title: "another scope".into(),
            },
        )
        .await?;
        let unrelated = node
            .fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
            .await?;
        assert_eq!(unrelated.state, SyncState::Complete);
        assert!(!unrelated.changed);
        let external = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
        external.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            rusqlite::params![vec![b'x'; content.len()], network, blob.hash],
        )?;
        let corrupt = node
            .fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
            .await?;
        assert_ne!(corrupt.state, SyncState::Complete);
        assert_eq!(corrupt.missing.blobs, vec![blob.clone()]);
        assert!(corrupt.changed);
        node.store.lock().await.put_blob(&network, &blob, content)?;
        assert_eq!(
            node.fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
                .await?
                .state,
            SyncState::Complete
        );
        let mut fork: Event = serde_json::from_str(&parent.canonical_body)?;
        fork.body = EventBody::MessagePosted {
            message_id: "fork".into(),
            role: "user".into(),
            text: "fork".into(),
            scheduler_node: None,
        };
        node.store
            .lock()
            .await
            .ingest_event(&network, &node.identity.sign_event(fork)?)?;
        let conflicted = node
            .fetch_scope(&network, "chat", &local, std::slice::from_ref(&child.hash))
            .await?;
        assert_eq!(conflicted.state, SyncState::Partial);
        assert!(conflicted.missing.conflicts.contains(&parent.hash));
        assert!(
            node.store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        {
            let store = node.store.lock().await;
            for scope in store.scopes(&network)? {
                assert!(
                    store
                        .scheduler_jobs(&network, &scope, &node.identity.peer_id()?)?
                        .is_empty()
                );
            }
            assert!(store.scheduler_outbox_all(&network)?.is_empty());
        }
        drop(external);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn net1_changing_heads_keep_one_scope_checkpoint_and_ignore_unverified_heads()
    -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.peer_id()?;
        let content = b"one missing blob across advancing heads";
        let blob = BlobRef {
            hash: crypto::digest(content),
            size: content.len() as u64,
        };
        let mut head = node.store.lock().await.heads(&network, "chat")?[0].clone();
        for sequence in 2..=130 {
            let event = node.identity.sign_event(Event {
                version: 1,
                network_id: network.clone(),
                scope_id: "chat".into(),
                author: local.clone(),
                sequence,
                previous: Some(head.clone()),
                parents: vec![head],
                created_at_ms: sequence as i64,
                body: EventBody::MessagePosted {
                    message_id: format!("message-{sequence}"),
                    role: "user".into(),
                    text: "waiting for one blob".into(),
                    scheduler_node: None,
                },
                blobs: vec![blob.clone()],
            })?;
            head = event.hash.clone();
            node.store.lock().await.ingest_event(&network, &event)?;
            let result = node
                .fetch_scope(&network, "chat", &local, std::slice::from_ref(&head))
                .await?;
            assert_eq!(result.missing.blobs, vec![blob.clone()]);
        }
        let other = LocalIdentity::generate();
        node.install_membership(test_owner(&node)?.sign_network_fixture(
            2,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                other.certificate(node.config.roles.clone())?,
            ],
        )?)
        .await?;
        let branch = other.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: other.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 1,
            body: EventBody::ConversationCreated {
                title: "local branch".into(),
            },
            blobs: vec![],
        })?;
        node.store.lock().await.ingest_event(&network, &branch)?;
        node.fetch_scope(&network, "chat", &local, std::slice::from_ref(&head))
            .await?;
        let key = format!("net1:v1:closure:{}", crypto::digest(b"chat"));
        let cursor = crypto::digest(b"retained actual-attempt cursor");
        let mut checkpoint = node
            .store
            .lock()
            .await
            .local_value(&network, &key)?
            .unwrap();
        checkpoint["event_cursor"] = json!(cursor);
        node.store
            .lock()
            .await
            .local_set(&network, &key, &checkpoint)?;
        node.fetch_closure_batch(&network, "chat", &local, std::slice::from_ref(&head), 0)
            .await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .local_value(&network, &key)?
                .unwrap()["event_cursor"],
            json!(cursor)
        );
        let ancestor = node
            .store
            .lock()
            .await
            .event(&network, &head)?
            .map(|envelope| serde_json::from_str::<Event>(&envelope.canonical_body))
            .transpose()?
            .unwrap()
            .previous
            .unwrap();
        node.fetch_closure_batch(&network, "chat", &local, &[ancestor], 0)
            .await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .local_value(&network, &key)?
                .unwrap()["event_cursor"],
            json!(cursor)
        );
        for index in 0..129 {
            let unknown = crypto::digest(format!("unverified-{index}").as_bytes());
            assert_eq!(
                node.fetch_scope(&network, "unverified", &local, &[unknown])
                    .await?
                    .state,
                SyncState::Unreachable
            );
        }
        let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
        let count: usize = connection.query_row(
            "SELECT count(*) FROM local_values WHERE key LIKE 'net1:v1:closure:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        node.store.lock().await.put_blob(&network, &blob, content)?;
        assert_eq!(
            node.fetch_scope(&network, "chat", &local, std::slice::from_ref(&head))
                .await?
                .state,
            SyncState::Complete
        );
        assert_eq!(
            node.store.lock().await.messages(&network, "chat")?.len(),
            129
        );
        assert!(
            !node
                .fetch_closure(&network, "chat", &local, std::slice::from_ref(&head))
                .await?
                .changed
        );
        assert_eq!(
            connection.query_row(
                "SELECT count(*) FROM local_values WHERE key LIKE 'net1:v1:closure:%'",
                [],
                |row| row.get::<_, usize>(0)
            )?,
            1
        );
        drop(connection);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn net1_checkpoints_require_matching_signed_scopes_and_allow_129_scopes() -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.peer_id()?;
        let content = b"one object for many real scopes";
        let blob = BlobRef {
            hash: crypto::digest(content),
            size: content.len() as u64,
        };
        let envelopes = (0..129)
            .map(|index| {
                node.identity.sign_event(Event {
                    version: 1,
                    network_id: network.clone(),
                    scope_id: format!("scope-{index:03}"),
                    author: local.clone(),
                    sequence: 1,
                    previous: None,
                    parents: vec![],
                    created_at_ms: 1,
                    body: EventBody::ConversationCreated {
                        title: "a real scope".into(),
                    },
                    blobs: vec![blob.clone()],
                })
            })
            .collect::<Result<Vec<_>>>()?;
        {
            let mut store = node.store.lock().await;
            store.ingest_events(&network, &envelopes)?;
            for (index, envelope) in envelopes.iter().enumerate() {
                let scope = format!("scope-{index:03}");
                let progress = SyncProgress::new(
                    &scope,
                    vec![envelope.hash.clone()],
                    &MissingDependencies {
                        events: vec![],
                        blobs: vec![blob.clone()],
                        conflicts: vec![],
                    },
                );
                save_sync_progress(
                    &mut store,
                    &network,
                    &format!("net1:v1:closure:{}", crypto::digest(scope.as_bytes())),
                    &progress,
                    true,
                )?;
                let forged = SyncProgress::new(
                    &format!("forged-{index}"),
                    vec![envelope.hash.clone()],
                    &MissingDependencies::default(),
                );
                let error = save_sync_progress(
                    &mut store,
                    &network,
                    &format!(
                        "net1:v1:closure:{}",
                        crypto::digest(forged.scope.as_bytes())
                    ),
                    &forged,
                    true,
                )
                .unwrap_err();
                assert_eq!(error.to_string(), "history head scope mismatch");
            }
        }
        let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
        assert_eq!(
            connection.query_row(
                "SELECT count(*) FROM local_values WHERE key LIKE 'net1:v1:closure:%'",
                [],
                |row| row.get::<_, usize>(0)
            )?,
            129
        );
        let heads = vec![envelopes[128].hash.clone(); 1025];
        assert_eq!(
            node.fetch_scope(&network, "scope-128", &local, &heads)
                .await?
                .missing
                .blobs,
            vec![blob.clone()]
        );
        node.store.lock().await.put_blob(&network, &blob, content)?;
        assert_eq!(
            node.fetch_scope(&network, "scope-128", &local, &heads)
                .await?
                .state,
            SyncState::Complete
        );
        assert_eq!(node.store.lock().await.conversations(&network)?.len(), 130);
        drop(connection);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn net1_closure_switches_authorized_replicas_and_isolates_bad_scopes() -> Result<()> {
        use axum::{
            Router,
            extract::{State, WebSocketUpgrade},
            routing::get,
        };
        async fn upgrade(
            State(hub): State<Arc<PeerHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(crate::peer::MAX_PACKET_BYTES)
                .max_frame_size(crate::peer::MAX_PACKET_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }
        let mut dirs = Vec::new();
        let mut nodes = Vec::new();
        for index in 0..4 {
            let dir = fixture_under("network-reliability")?;
            if index == 0 {
                configure_manager(&dir)?;
            }
            nodes.push(Arc::new(Runtime::open(&dir).await?));
            dirs.push(dir);
        }
        let owner = test_owner(&nodes[0])?;
        let state = owner.sign_network_fixture(
            1,
            nodes
                .iter()
                .map(|node| node.identity.certificate(node.config.roles.clone()))
                .collect::<Result<Vec<_>>>()?,
        )?;
        let network = state.genesis.network_id.clone();
        for node in &nodes {
            node.install_membership(state.clone()).await?;
        }
        let author = &nodes[1].identity;
        let root = author.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: author.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 1,
            body: EventBody::ConversationCreated {
                title: "distributed".into(),
            },
            blobs: vec![],
        })?;
        let bytes = b"from a third authorized replica";
        let blob = BlobRef {
            hash: crypto::digest(bytes),
            size: bytes.len() as u64,
        };
        let head = author.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: author.peer_id()?,
            sequence: 2,
            previous: Some(root.hash.clone()),
            parents: vec![root.hash.clone()],
            created_at_ms: 2,
            body: EventBody::MessagePosted {
                message_id: "distributed".into(),
                role: "user".into(),
                text: "complete once".into(),
                scheduler_node: None,
            },
            blobs: vec![blob.clone()],
        })?;
        nodes[1].store.lock().await.ingest_event(&network, &head)?;
        nodes[2].store.lock().await.ingest_event(&network, &root)?;
        nodes[3]
            .store
            .lock()
            .await
            .put_blob(&network, &blob, bytes)?;
        let membership_during_fetch = Arc::new(Mutex::new(None::<NetworkState>));
        let mut tasks = tokio::task::JoinSet::new();
        for (index, source) in nodes.iter().enumerate().skip(1).rev() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("ws://{}/peer", listener.local_addr()?);
            let router = Router::new()
                .route("/peer", get(upgrade))
                .with_state(source.hub.clone());
            tasks.spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            let hub = nodes[0].hub.clone();
            let scope = network.clone();
            tasks.spawn(async move {
                let _ = hub.connect(scope, url).await;
            });
            let source = source.clone();
            let mut inbound = source
                .peer_rx
                .lock()
                .await
                .take()
                .context("missing test receiver")?;
            let wrong = head.clone();
            let root_hash = root.hash.clone();
            let scope = network.clone();
            let membership_during_fetch = membership_during_fetch.clone();
            let receiver = nodes[0].clone();
            tasks.spawn(async move {
                while let Some(message) = inbound.recv().await {
                    if message.payload["type"] == "event" {
                        let update = membership_during_fetch.lock().await.take();
                        if let Some(update) = update { receiver.install_membership(update).await.unwrap(); }
                    }
                    let value = if message.payload["type"] == "capabilities" {
                        let mut capabilities = source.capabilities();
                        capabilities.as_object_mut().unwrap().remove("history_batch");
                        capabilities
                    } else if index == 1 && message.payload["type"] == "event" && message.payload["hash"] == root_hash {
                        json!({"event":wrong})
                    } else if index == 1 && message.payload["type"] == "inventory" {
                        json!({"membership":source.store.lock().await.trust_bundle(&scope).unwrap(),"scopes":[
                            {"scope_id":"broken","heads":[crypto::digest(b"unavailable event")]},
                            {"heads":[]}, {"scope_id":"chat","heads":[wrong.hash]}]})
                    } else {
                        source.handle_peer(&message).await.unwrap_or_else(|error| json!({"error":error.to_string()}))
                    };
                    let _ = source.hub.respond(&message, value).await;
                }
            });
            if index == 3 {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !nodes[0]
                        .hub
                        .online(&network)
                        .await
                        .contains(&nodes[3].identity.peer_id().unwrap())
                    {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await?;
                let empty = nodes[0].sync_all(&network).await?;
                assert_eq!(empty.state, SyncState::Complete);
                assert!(empty.scopes.is_empty());
            }
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while nodes[0].hub.online(&network).await.len() < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        let preferred = nodes[1].identity.peer_id()?;
        let partial = nodes[0]
            .fetch_closure_batch(
                &network,
                "chat",
                &preferred,
                std::slice::from_ref(&head.hash),
                1,
            )
            .await?;
        assert_eq!(partial.state, SyncState::Partial);
        assert_eq!(partial.missing.events, vec![root.hash.clone()]);
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .event(&network, &head.hash)?
                .is_some()
        );
        let reports = nodes[0].sync_from(&network, &preferred).await?;
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0].state, SyncState::Unreachable);
        assert_eq!(reports[1].state, SyncState::Partial);
        assert_eq!(reports[2].state, SyncState::Complete);
        assert_eq!(
            nodes[0].store.lock().await.get_blob(&network, &blob.hash)?,
            Some(bytes.to_vec())
        );
        assert_eq!(
            nodes[0].store.lock().await.messages(&network, "chat")?[0].text,
            "complete once"
        );
        let noop = nodes[0]
            .fetch_scope(
                &network,
                "chat",
                &preferred,
                std::slice::from_ref(&head.hash),
            )
            .await?;
        assert_eq!(noop.state, SyncState::Complete);
        assert!(!noop.changed);
        {
            let store = nodes[0].store.lock().await;
            for scope in store.scopes(&network)? {
                assert!(
                    store
                        .scheduler_jobs(&network, &scope, &nodes[0].identity.peer_id()?)?
                        .is_empty()
                );
            }
            assert!(store.scheduler_outbox_all(&network)?.is_empty());
        }
        let fifth_bytes = b"fifth blob makes progress after four unavailable objects";
        let fifth = BlobRef {
            hash: crypto::digest(fifth_bytes),
            size: fifth_bytes.len() as u64,
        };
        let mut refs = (1..=4)
            .map(|index| BlobRef {
                hash: format!("{:064x}", index),
                size: 1,
            })
            .collect::<Vec<_>>();
        refs.push(fifth.clone());
        refs.sort();
        assert_eq!(refs[4], fifth);
        nodes[3]
            .store
            .lock()
            .await
            .put_blob(&network, &fifth, fifth_bytes)?;
        let starvation = author.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "starvation".into(),
            author: author.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 1,
            body: EventBody::ConversationCreated {
                title: "rotation".into(),
            },
            blobs: refs,
        })?;
        nodes[1]
            .store
            .lock()
            .await
            .ingest_event(&network, &starvation)?;
        let first_page = nodes[0]
            .fetch_scope(
                &network,
                "starvation",
                &preferred,
                std::slice::from_ref(&starvation.hash),
            )
            .await?;
        assert_eq!(first_page.state, SyncState::Partial);
        assert_eq!(first_page.missing.blobs.len(), 5);
        assert!(!nodes[0].store.lock().await.has_blob(&network, &fifth)?);
        let next_page = nodes[0]
            .fetch_scope(
                &network,
                "starvation",
                &preferred,
                std::slice::from_ref(&starvation.hash),
            )
            .await?;
        assert_eq!(next_page.state, SyncState::Partial);
        assert_eq!(next_page.missing.blobs.len(), 4);
        assert_eq!(
            nodes[0]
                .store
                .lock()
                .await
                .get_blob(&network, &fifth.hash)?,
            Some(fifth_bytes.to_vec())
        );
        let wrong_scope = author.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "proper-root".into(),
            author: author.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 1,
            body: EventBody::ConversationCreated {
                title: "proper scope".into(),
            },
            blobs: vec![],
        })?;
        nodes[1]
            .store
            .lock()
            .await
            .ingest_event(&network, &wrong_scope)?;
        assert_eq!(
            nodes[0]
                .fetch_scope(
                    &network,
                    "forged-root",
                    &preferred,
                    std::slice::from_ref(&wrong_scope.hash)
                )
                .await
                .unwrap_err()
                .to_string(),
            "history head scope mismatch"
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .event(&network, &wrong_scope.hash)?
                .is_none()
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .local_value(
                    &network,
                    &format!("net1:v1:closure:{}", crypto::digest(b"forged-root"))
                )?
                .is_none()
        );
        let dispatch = author.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: author.peer_id()?,
            sequence: 3,
            previous: Some(head.hash.clone()),
            parents: vec![head.hash.clone()],
            created_at_ms: 3,
            body: EventBody::MessagePosted {
                message_id: "revoked-before-schedule".into(),
                role: "user".into(),
                text: "do not start an Agent after revocation".into(),
                scheduler_node: Some(nodes[0].identity.peer_id()?),
            },
            blobs: vec![],
        })?;
        nodes[3]
            .store
            .lock()
            .await
            .ingest_event(&network, &dispatch)?;
        let revoked_state = |epoch: u64, second: bool| -> Result<NetworkState> {
            owner.sign_network_fixture(
                epoch,
                nodes
                    .iter()
                    .enumerate()
                    .map(|(index, node)| {
                        let mut certificate =
                            node.identity.certificate(node.config.roles.clone())?;
                        certificate.revoked = index == 1 || (second && index == 2);
                        Ok(certificate)
                    })
                    .collect::<Result<Vec<_>>>()?,
            )
        };
        *membership_during_fetch.lock().await = Some(revoked_state(2, false)?);
        let execute = Inbound {
            network_id: network.clone(),
            origin: preferred.clone(),
            request_id: "revoked-dispatch".into(),
            broadcast: false,
            payload: json!({"type":"schedule","version":1,"target_node":nodes[0].identity.peer_id()?,"conversation_id":"chat","trigger":dispatch.hash,"basis_heads":[dispatch.hash]}),
        };
        let error = nodes[0].handle_peer(&execute).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "identity is not authorized in this personal network"
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .context(&network, "chat", std::slice::from_ref(&dispatch.hash))?
                .missing
                .is_complete()
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .scheduler_job(
                    &network,
                    "chat",
                    &dispatch.hash,
                    &nodes[0].identity.peer_id()?
                )?
                .is_none()
        );
        let schedule_author = &nodes[3].identity;
        let (trigger, schedule_basis) = {
            let mut store = nodes[3].store.lock().await;
            store.append_event(
                schedule_author,
                &network,
                "schedule-revocation",
                EventBody::ConversationCreated {
                    title: "schedule revocation".into(),
                },
                vec![],
                1,
            )?;
            let (trigger, _) = store.accept_message(
                schedule_author,
                &network,
                "schedule-revocation",
                EventBody::MessagePosted {
                    message_id: "revoked-schedule".into(),
                    role: "user".into(),
                    text: "do not run".into(),
                    scheduler_node: Some(nodes[0].identity.peer_id()?),
                },
                vec![],
                2,
                &schedule_author.peer_id()?,
                &nodes[0].identity.peer_id()?,
                false,
            )?;
            (trigger, store.heads(&network, "schedule-revocation")?)
        };
        *membership_during_fetch.lock().await = Some(revoked_state(3, true)?);
        let schedule = Inbound {
            network_id: network.clone(),
            origin: nodes[2].identity.peer_id()?,
            request_id: "revoked-schedule".into(),
            broadcast: false,
            payload: json!({"type":"schedule","version":1,"target_node":nodes[0].identity.peer_id()?,"conversation_id":"schedule-revocation","trigger":trigger.hash,"basis_heads":schedule_basis}),
        };
        let error = nodes[0].handle_peer(&schedule).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "identity is not authorized in this personal network"
        );
        assert!(
            nodes[0]
                .store
                .lock()
                .await
                .context(
                    &network,
                    "schedule-revocation",
                    std::slice::from_ref(&trigger.hash)
                )?
                .missing
                .is_complete()
        );
        assert_eq!(
            nodes[0].store.lock().await.scheduler_job(
                &network,
                "schedule-revocation",
                &trigger.hash,
                &nodes[0].identity.peer_id()?
            )?,
            None
        );
        let (snapshot, membership) = {
            let store = nodes[0].store.lock().await;
            store.authorize_node(
                &network,
                &schedule_author.peer_id()?,
                Some(NodeRole::Gateway),
            )?;
            assert!(
                store
                    .report_intent_head(&network, "schedule-revocation", &trigger.hash)?
                    .is_some()
            );
            (
                store.export_snapshot(&network, "schedule-revocation")?,
                store.trust_bundle(&network)?.unwrap(),
            )
        };
        let mut admission_control = Store::open(":memory:")?;
        admission_control.install_trust_bundle(&network, &membership)?;
        assert!(
            admission_control
                .import_snapshot(&network, &snapshot)?
                .is_complete()
        );
        admission_control.enqueue_trigger(
            &network,
            "schedule-revocation",
            &trigger.hash,
            &nodes[0].identity.peer_id()?,
        )?;
        assert_eq!(
            admission_control
                .scheduler_job(
                    &network,
                    "schedule-revocation",
                    &trigger.hash,
                    &nodes[0].identity.peer_id()?
                )?
                .unwrap()
                .state
                .name(),
            "queued"
        );
        let local_scope = "local-replica-revocation";
        let local_root = nodes[3].store.lock().await.append_event(
            &nodes[3].identity,
            &network,
            local_scope,
            EventBody::ConversationCreated {
                title: "local import authorization".into(),
            },
            vec![],
            4,
        )?;
        let (local_dispatch, _) = nodes[3].store.lock().await.accept_message(
            &nodes[3].identity,
            &network,
            local_scope,
            EventBody::MessagePosted {
                message_id: "local-role-revoked-schedule".into(),
                role: "user".into(),
                text: "do not start an Agent after the local Replica role is revoked".into(),
                scheduler_node: Some(nodes[0].identity.peer_id()?),
            },
            vec![],
            5,
            &nodes[3].identity.peer_id()?,
            &nodes[0].identity.peer_id()?,
            false,
        )?;
        let local_basis = nodes[3].store.lock().await.heads(&network, local_scope)?;
        let update = owner.sign_network_fixture(
            4,
            nodes
                .iter()
                .enumerate()
                .map(|(index, node)| {
                    let roles = if index == 0 {
                        vec![NodeRole::Relay]
                    } else {
                        node.config.roles.clone()
                    };
                    let mut certificate = node.identity.certificate(roles)?;
                    certificate.revoked = index == 1 || index == 2;
                    Ok(certificate)
                })
                .collect::<Result<Vec<_>>>()?,
        )?;
        *membership_during_fetch.lock().await = Some(update);
        let execute = Inbound {
            network_id: network.clone(),
            origin: nodes[3].identity.peer_id()?,
            request_id: "local-role-revoked-dispatch".into(),
            broadcast: false,
            payload: json!({"type":"schedule","version":1,"target_node":nodes[0].identity.peer_id()?,"conversation_id":local_scope,"trigger":local_dispatch.hash,"basis_heads":local_basis}),
        };
        let error = nodes[0].handle_peer(&execute).await.unwrap_err();
        assert_eq!(error.to_string(), "Node lacks required role");
        assert!(membership_during_fetch.lock().await.is_none());
        let store = nodes[0].store.lock().await;
        store.authorize_node(
            &network,
            &nodes[0].identity.peer_id()?,
            Some(NodeRole::Relay),
        )?;
        assert!(
            store
                .authorize_node(
                    &network,
                    &nodes[0].identity.peer_id()?,
                    Some(NodeRole::Replica)
                )
                .is_err()
        );
        assert!(
            store.event(&network, &local_dispatch.hash)?.is_none(),
            "revoked local Replica must not import the received page"
        );
        assert!(store.event(&network, &local_root.hash)?.is_none());
        assert!(
            store
                .scheduler_job(
                    &network,
                    local_scope,
                    &local_dispatch.hash,
                    &nodes[0].identity.peer_id()?
                )?
                .is_none()
        );
        drop(store);
        println!(
            "NET1_M5 local_replica_role_removed_during_real_noise_response=true local_relay_membership_retained=true received_page_imports=0 scheduler_admissions=0 sdk_starts=0"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(nodes);
        for dir in dirs {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    fn configure_manager(dir: &Path) -> Result<()> {
        let mut config = Config::load(dir)?;
        config.manager = Some(config::ManagerSettings {
            provider: "cursor_sdk".into(),
            workspace: dir.into(),
            model: "auto".into(),
            cursor_sdk_bin: dir.join("must-not-spawn"),
            node_bin: "node".into(),
            codex_sdk_script: dir.join("must-not-run.mjs"),
        });
        config.save(dir)
    }
    async fn task(node: &Arc<Runtime>, network: &str, task_id: &str, report: bool) -> Result<()> {
        let access = Access {
            network_id: network.into(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        node.rpc(
            access.clone(),
            json!({"method":"create_task","conversation_id":"chat","task_id":task_id,
                "title":"test","goal":"test","acceptance":"user verifies"}),
        )
        .await?;
        if report {
            let version = node
                .store
                .lock()
                .await
                .followup_policy(network, "chat", task_id)?
                .version;
            node.rpc(
                access,
                json!({"method":"set_followup","conversation_id":"chat",
                "task_id":task_id,"mode":"report","expected_version":version,
                "scheduler_node":node.identity.peer_id()?}),
            )
            .await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn unsupported_epoch_does_not_open_databases_or_modify_the_directory() -> Result<()> {
        let dir = fixture()?;
        let config = std::fs::read(dir.join("config.json"))?;
        for epoch in [None, Some(1), Some(2), Some(3), Some(4), Some(6)] {
            if let Some(epoch) = epoch {
                config::private_write(
                    &dir.join("data-epoch.json"),
                    &serde_json::to_vec(&json!({"data_epoch":epoch}))?,
                )?;
            } else {
                std::fs::remove_file(dir.join("data-epoch.json"))?;
            }
            let marker = std::fs::read(dir.join("data-epoch.json")).ok();
            assert!(Runtime::open(&dir).await.is_err());
            assert!(!dir.join("node.sqlite3").exists());
            assert!(!dir.join("identity.sqlite3").exists());
            assert_eq!(std::fs::read(dir.join("config.json"))?, config);
            assert_eq!(std::fs::read(dir.join("data-epoch.json")).ok(), marker);
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn membership_reopen_uses_the_single_store_without_root_secrets() -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        assert!(!node.has_membership().await?);
        let owner = test_owner(&node)?;
        let initial = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        node.store.lock().await.install_state(initial.clone())?;
        drop(node);
        let node = Runtime::open(&dir).await?;
        assert_eq!(node.membership_state().await?, initial);
        assert!(node.has_membership().await?);
        let updated = owner.sign_network_fixture(
            2,
            vec![
                node.identity
                    .certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
            ],
        )?;
        node.store.lock().await.install_state(updated.clone())?;
        drop(node);
        let node = Runtime::open(&dir).await?;
        assert_eq!(node.membership_state().await?, updated);
        assert_eq!(node.store.lock().await.public_state()?, updated);
        assert!(!dir.join("auth.sqlite3").exists());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_cannot_reopen_or_start_new_work_for_a_user_finalized_task() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        task(&node, &network, "task", false).await?;
        let human = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: Some("chat".into()),

            manager: None,
        };
        let agent = Access {
            human: false,
            ..human.clone()
        };
        for status in ["accepted", "cancelled"] {
            node.rpc(human.clone(), json!({"method":"update_task","conversation_id":"chat","task_id":"task","status":status})).await?;
            let update = json!({"method":"update_task","conversation_id":"chat","task_id":"task","status":"working"});
            assert!(
                node.rpc(agent.clone(), update.clone())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("only the user may reopen")
            );
            node.rpc(human.clone(), update).await?;
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn peer_mutations_and_replica_reads_enforce_roles() -> Result<()> {
        let dir = fixture()?;
        configure_manager(&dir)?;
        let mut node = Arc::new(Runtime::open(&dir).await?);
        let owner = LocalIdentity::generate();
        let replica = LocalIdentity::generate();
        let relay = LocalIdentity::generate();
        let scheduler = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        node.install_membership(owner.sign_network_fixture(
            1,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                owner.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
                replica.certificate(vec![NodeRole::Replica])?,
                relay.certificate(vec![NodeRole::Relay])?,
                scheduler.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
            ],
        )?)
        .await?;
        let event = node
            .append(
                &network,
                "chat",
                EventBody::ConversationCreated {
                    title: "roles".into(),
                },
            )
            .await?;
        let missing = crypto::digest(b"not present on the receiver");
        let schedule = json!({"type":"schedule","version":1,"target_node":node.identity.peer_id()?,
            "conversation_id":"chat","trigger":missing,"basis_heads":[missing]});
        let resolution = json!({"type":"resolve_manager_unknown","conversation_id":"chat","trigger":"missing","human":true,"evidence":"verified fixture stopped"});
        for origin in [&replica, &relay] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.peer_id()?,
                request_id: "role-test".into(),
                payload: resolution.clone(),
                broadcast: false,
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                "Node lacks required role"
            );
            let message = Inbound {
                payload: schedule.clone(),
                ..message
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                "origin cannot schedule"
            );
        }
        for origin in [&owner, &scheduler] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.peer_id()?,
                request_id: "schedule-role-test".into(),
                payload: schedule.clone(),
                broadcast: false,
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                "history is Unreachable: 1 events, 0 blobs, 0 conflicts"
            );
        }
        let message = Inbound {
            network_id: network.clone(),
            origin: owner.peer_id()?,
            request_id: "gateway-test".into(),
            payload: resolution,
            broadcast: false,
        };
        let message = Inbound {
            origin: replica.peer_id()?,
            payload: json!({"type":"event","hash":event.hash}),
            ..message
        };
        assert_eq!(
            node.handle_peer(&message).await?["event"]["hash"],
            event.hash
        );
        let blob = BlobRef {
            hash: crypto::digest(b"test"),
            size: 4,
        };
        node.store.lock().await.put_blob(&network, &blob, b"test")?;
        let blob_message = Inbound {
            payload: json!({"type":"blob","hash":blob.hash,"offset":0}),
            ..message.clone()
        };
        assert_eq!(node.handle_peer(&blob_message).await?["data"], "74657374");
        Arc::get_mut(&mut node)
            .unwrap()
            .config
            .roles
            .retain(|r| r != &NodeRole::Replica);
        for message in [&message, &blob_message] {
            assert_eq!(
                node.handle_peer(message).await.unwrap_err().to_string(),
                "node capability is disabled"
            );
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_message_retries_record_one_trigger() -> Result<()> {
        let dir = fixture()?;
        configure_manager(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let wrong_entry = node.rpc(Access { network_id: network.clone(), human: true, conversation_id: None, manager: None },
            json!({"method":"message","conversation_id":"chat","message_id":"wrong-entry","text":"same goal","entry_node":"another-gateway"})).await.unwrap_err();
        assert!(wrong_entry.to_string().contains("original gateway"));
        assert!(
            node.store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut calls = Vec::new();
        for _ in 0..16 {
            let node = node.clone();
            let network = network.clone();
            let barrier = barrier.clone();
            calls.push(tokio::spawn(async move {
                    barrier.wait().await;
                    node.rpc(Access { network_id: network, human: true, conversation_id: None,
    manager: None,
    },
                        json!({"method":"message", "conversation_id":"chat", "message_id":"same-message", "text":"same goal"})).await
                }));
        }
        let mut triggers = BTreeSet::new();
        for call in calls {
            let receipt = call.await??;
            assert_eq!(receipt["state"], "queued");
            assert_eq!(receipt["queued"], true);
            assert_eq!(receipt["receipt"]["target_node"], node.identity.peer_id()?);
            triggers.insert(receipt["trigger"].as_str().unwrap().to_owned());
        }
        assert_eq!(triggers.len(), 1);
        let count = node.store.lock().await.context(&network, "chat", &[])?.events.iter()
                .filter(|event| matches!(&event.body, EventBody::MessagePosted { message_id, .. } if message_id == "same-message")).count();
        assert_eq!(count, 1);
        assert_eq!(
            node.store
                .lock()
                .await
                .scheduler_jobs(&network, "chat", &node.identity.peer_id()?)?
                .len(),
            1
        );
        {
            let mut store = node.store.lock().await;
            let head = store.heads(&network, "chat")?.remove(0);
            let envelope = store.event(&network, &head)?.context("head missing")?;
            let mut event = store.verify_event(&network, &envelope)?;
            event.sequence += 1;
            event.previous = Some(head.clone());
            event.parents = vec![head, "ab".repeat(32)];
            event.body = EventBody::ContextCheckpoint {
                summary: "child arrived before its other parent".into(),
                covered_heads: vec![],
            };
            store.ingest_event(&network, &node.identity.sign_event(event)?)?;
            assert!(!store.context(&network, "chat", &[])?.missing.is_complete());
        }
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        let duplicate = node.rpc(access.clone(), json!({"method":"message","conversation_id":"chat","message_id":"same-message","text":"same goal"})).await?;
        assert!(triggers.contains(duplicate["trigger"].as_str().context("trigger missing")?));
        let changed = node.rpc(access.clone(), json!({"method":"message","conversation_id":"chat","message_id":"same-message","text":"changed goal"})).await.unwrap_err();
        assert!(changed.to_string().contains("different content"));
        let changed_intent = node.rpc(access.clone(), json!({"method":"message","conversation_id":"chat","message_id":"same-message","text":"same goal","report_results":false})).await.unwrap_err();
        assert!(
            changed_intent
                .to_string()
                .contains("cannot change its follow-up intent")
        );
        let fresh = node.rpc(access, json!({"method":"message","conversation_id":"chat","message_id":"new-message","text":"new goal"})).await.unwrap_err();
        assert!(fresh.to_string().contains("context is incomplete"));
        assert_eq!(
            node.store
                .lock()
                .await
                .scheduler_jobs(&network, "chat", &node.identity.peer_id()?)?
                .len(),
            1
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn task_retry_requires_explicit_versioned_grant_without_reopening() -> Result<()> {
        use crate::store::followup::FollowupMode;
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        for (task_id, status) in [
            ("active", "open"),
            ("accepted", "accepted"),
            ("cancelled", "cancelled"),
        ] {
            task(&node, &network, task_id, false).await?;
            if status != "open" {
                node.append(
                    &network,
                    "chat",
                    EventBody::TaskChanged {
                        task_id: task_id.into(),
                        status: status.into(),
                        detail: "user ended this task".into(),
                    },
                )
                .await?;
            }
        }
        drop(node);
        let node = Arc::new(Runtime::open(&dir).await?);
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        for _ in 0..2 {
            for task_id in ["active", "accepted", "cancelled"] {
                let rejected = node.rpc(access.clone(), json!({"method":"create_task","conversation_id":"chat",
                    "task_id":task_id,"title":"test","goal":"test","acceptance":"user verifies","auto_followup":true})).await.unwrap_err();
                assert!(rejected.to_string().contains("task follow-up control"));
                task(&node, &network, task_id, false).await?;
                assert_eq!(
                    node.store
                        .lock()
                        .await
                        .followup_policy(&network, "chat", task_id)?
                        .version,
                    0
                );
            }
        }
        let grant = json!({"method":"set_followup","conversation_id":"chat","task_id":"active",
            "mode":"report","expected_version":0,"scheduler_node":node.identity.peer_id()?});
        assert_eq!(
            node.rpc(access.clone(), grant.clone()).await?["policy"]["version"],
            1
        );
        assert!(node.rpc(access, grant).await.is_err());
        let store = node.store.lock().await;
        assert_eq!(
            store.followup_policy(&network, "chat", "active")?.mode,
            FollowupMode::Report
        );
        for (task_id, status) in [("accepted", "accepted"), ("cancelled", "cancelled")] {
            assert_eq!(
                store.followup_policy(&network, "chat", task_id)?.mode,
                FollowupMode::Off
            );
            assert_eq!(
                store
                    .tasks(&network, "chat")?
                    .into_iter()
                    .find(|task| task.task_id == task_id)
                    .unwrap()
                    .status,
                status
            );
        }
        let grants = store
            .context(&network, "chat", &[])?
            .events
            .into_iter()
            .filter(|event| {
                matches!(&event.body, EventBody::DecisionRecorded { tool_configuration, .. }
                if tool_configuration["type"] == "task_followup_grant_v1")
            })
            .count();
        assert_eq!(grants, 1);
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn retrying_an_undelivered_message_reuses_its_event_and_pending_delivery() -> Result<()> {
        let dir = fixture()?;
        configure_manager(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        let mut request = json!({"method":"message","conversation_id":"chat","message_id":"retry-message","text":"test","scheduler_node":"unregistered-node"});
        assert!(node.rpc(access.clone(), request.clone()).await.is_err());
        assert!(
            node.store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        let target = LocalIdentity::generate();
        let other = LocalIdentity::generate();
        let owner = test_owner(&node)?;
        node.install_membership(owner.sign_network_fixture(
            2,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                target.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
                other.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
            ],
        )?)
        .await?;
        request["scheduler_node"] = json!(target.peer_id()?);
        let first = node.rpc(access.clone(), request.clone()).await?;
        assert_eq!(first["state"], "pending_delivery");
        assert_eq!(first["accepted"], true);
        assert_eq!(first["queued"], false);
        let retry = node.rpc(access.clone(), request.clone()).await?;
        assert_eq!(retry["receipt"], first["receipt"]);
        assert_eq!(retry["trigger"], first["trigger"]);
        request["scheduler_node"] = json!(other.peer_id()?);
        let changed = node.rpc(access.clone(), request).await.unwrap_err();
        assert!(changed.to_string().contains("different content or target"));
        let store = node.store.lock().await;
        assert_eq!(store.messages(&network, "chat")?.len(), 1);
        let pending = store.scheduler_outbox(&network)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_node, target.peer_id()?);
        assert_eq!(pending[0].trigger, first["trigger"].as_str().unwrap());
        assert!(!pending[0].delivered);
        drop(store);
        let fresh = node.rpc(access, json!({"method":"message","conversation_id":"chat","message_id":"fresh-message","text":"new request"})).await?;
        assert_eq!(fresh["scheduler_node"], node.identity.peer_id()?);
        assert_ne!(fresh["scheduler_node"], first["scheduler_node"]);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
#[derive(Clone)]
pub struct Update {
    pub network_id: String,
    pub payload: Value,
}
const HISTORY_BATCH_EVENTS: usize = 128;
const HISTORY_BATCH_BYTES: usize = crate::peer::MAX_PACKET_BYTES - 1024;

struct HistorySource {
    peer_id: String,
    batch: Option<bool>,
    failed: bool,
}

struct HistoryBudget {
    events: usize,
    bytes: usize,
}
impl HistoryBudget {
    fn response(&mut self, value: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(value)?.len();
        let fits = bytes <= self.bytes;
        self.bytes = self.bytes.saturating_sub(bytes);
        ensure!(fits, "history response byte budget exceeded");
        Ok(())
    }
}

fn history_batch_capability() -> Value {
    json!({"version":1,"max_events":HISTORY_BATCH_EVENTS,"max_bytes":HISTORY_BATCH_BYTES})
}

fn verify_history_batch(
    store: &Store,
    network: &str,
    hash: &str,
    value: &Value,
    limit: usize,
    max_bytes: usize,
) -> Result<Vec<SignedEvent>> {
    ensure!(
        value["version"] == 1 && value["hash"] == hash,
        "history batch root/version mismatch"
    );
    ensure!(
        serde_json::to_vec(value)?.len() <= max_bytes,
        "history batch byte limit exceeded"
    );
    let values = value["events"]
        .as_array()
        .context("invalid history batch")?;
    ensure!(
        !values.is_empty() && values.len() <= limit,
        "history batch count limit exceeded"
    );
    let mut events = HashMap::new();
    for value in values {
        let envelope: SignedEvent = serde_json::from_value(value.clone())?;
        let event = store.verify_event(network, &envelope)?;
        ensure!(
            events
                .insert(envelope.hash.clone(), (envelope, event))
                .is_none(),
            "duplicate event in history batch"
        );
    }
    ensure!(events.contains_key(hash), "history batch root missing");
    let mut pending = VecDeque::from([hash.to_owned()]);
    let mut visited = BTreeSet::new();
    let mut envelopes = Vec::new();
    while let Some(hash) = pending.pop_front() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        if let Some((envelope, event)) = events.remove(&hash) {
            pending.extend(event.parents.into_iter().chain(event.previous));
            envelopes.push(envelope);
        }
    }
    ensure!(events.is_empty(), "unrelated event in history batch");
    Ok(envelopes)
}

#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct SyncProgress {
    version: u32,
    scope: String,
    heads: Vec<String>,
    events: Vec<String>,
    blobs: Vec<BlobRef>,
    event_cursor: Option<String>,
    blob_cursor: Option<BlobRef>,
    frontier_generation: Option<u64>,
}
impl SyncProgress {
    fn new(scope: &str, heads: Vec<String>, missing: &MissingDependencies) -> Self {
        let mut progress = Self {
            version: 1,
            scope: scope.into(),
            heads,
            events: Vec::new(),
            blobs: Vec::new(),
            event_cursor: None,
            blob_cursor: None,
            frontier_generation: None,
        };
        progress.advance(missing);
        progress
    }
    fn advance(&mut self, missing: &MissingDependencies) {
        self.events = rotated_page(&missing.events, self.event_cursor.as_ref(), 1024);
        self.blobs = rotated_page(&missing.blobs, self.blob_cursor.as_ref(), 128);
        if !missing.is_complete() {
            self.frontier_generation = None;
        }
    }
}
fn rotated_page<T: Ord + Clone>(items: &[T], cursor: Option<&T>, limit: usize) -> Vec<T> {
    let start = cursor.map_or(0, |cursor| items.partition_point(|item| item <= cursor));
    items[start..]
        .iter()
        .chain(items[..start].iter())
        .take(limit)
        .cloned()
        .collect()
}
fn save_sync_progress(
    store: &mut Store,
    network: &str,
    key: &str,
    progress: &SyncProgress,
    persist: bool,
) -> Result<()> {
    if !persist {
        return Ok(());
    }
    let mut verified_head = false;
    for hash in &progress.heads {
        if let Some(envelope) = store.event(network, hash)? {
            ensure!(
                store.verify_event(network, &envelope)?.scope_id == progress.scope,
                "history head scope mismatch"
            );
            verified_head = true;
        }
    }
    if !verified_head && store.local_value(network, key)?.is_none() {
        return Ok(());
    }
    let value = serde_json::to_value(progress)?;
    if store.local_value(network, key)?.as_ref() != Some(&value) {
        store.local_set(network, key, &value)?;
    }
    Ok(())
}

const MAX_ROUTE_FLIGHTS: usize = 128;

#[derive(Clone, Debug)]
enum RouteFailure {
    Denied(AuthorizationDenied),
    Busy,
    Unavailable(String),
}
impl RouteFailure {
    fn from_error(error: &anyhow::Error) -> Self {
        if let Some(error) = error.downcast_ref::<AuthorizationDenied>() {
            Self::Denied(error.clone())
        } else if error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy)
            || error.downcast_ref::<crate::network::Denied>().is_some()
        {
            Self::Busy
        } else {
            Self::Unavailable(format!("{error:#}"))
        }
    }
}

#[derive(Clone, Debug)]
enum RouteFlightState {
    Connecting,
    Ready(String),
    Failed(RouteFailure),
}
struct RouteFlight {
    id: String,
    state: watch::Sender<RouteFlightState>,
}

pub struct Runtime {
    pub config: Config,
    pub identity: LocalIdentity,
    pub store: Arc<Mutex<Store>>,
    pub network: Arc<NetworkHub>,
    pub hub: Arc<PeerHub>,
    route_flights: Arc<Mutex<HashMap<(String, String), RouteFlight>>>,
    sync: Arc<SyncCoordinator>,
    pub events: broadcast::Sender<Update>,
    pub agent_tokens: Mutex<HashMap<String, Access>>,
    manager_controls: Mutex<HashMap<String, Arc<manager_control::ActiveManager>>>,
    agent_blobs: Mutex<HashMap<agent_tools::AgentBlobKey, agent_tools::AgentBlobSnapshot>>,
    blob_staging: Arc<blob_transfer::BlobStaging>,
    schedule_workers: Mutex<BTreeSet<(String, String)>>,
    schedule_wakeup: tokio::sync::Notify,
    observation: std::sync::Mutex<observation::ObservationState>,
    pub tool_url: std::sync::OnceLock<String>,
    pub maintenance: Arc<Gate>,
    codex: Option<vibeface_bridge::codex::CodexClient>,
    pub local_access: Arc<crate::local_access::LocalAccess>,
    _codex_process: Option<tokio::process::Child>,
    _process_lock: Arc<std::fs::File>,
    peer_rx: Mutex<Option<mpsc::Receiver<Inbound>>>,
    network_rx: Mutex<Option<mpsc::Receiver<IncomingSocket>>>,
    native_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    let s = v[key]
        .as_str()
        .with_context(|| format!("{key} is required"))?;
    ensure!(!s.trim().is_empty() && s.len() <= 65536, "invalid {key}");
    Ok(s)
}
fn id(v: &Value, key: &str) -> Result<String> {
    let s = field(v, key)?;
    ensure!(
        s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)),
        "invalid {key}"
    );
    Ok(s.into())
}
fn heads(v: &Value) -> Result<Vec<String>> {
    Ok(serde_json::from_value(
        v.get("basis_heads").cloned().unwrap_or(json!([])),
    )?)
}
fn require_task_write(
    store: &Store,
    network: &str,
    scope: &str,
    task_id: &str,
    human: bool,
) -> Result<()> {
    let task = store
        .tasks(network, scope)?
        .into_iter()
        .find(|t| t.task_id == task_id)
        .context("task not found")?;
    ensure!(
        human || !matches!(task.status.as_str(), "accepted" | "cancelled"),
        "only the user may reopen accepted or cancelled tasks"
    );
    Ok(())
}

impl Runtime {
    pub async fn open(dir: &Path) -> Result<Self> {
        let epoch: Value = serde_json::from_slice(&std::fs::read(dir.join("data-epoch.json"))?)?;
        ensure!(
            epoch == json!({"data_epoch":5}),
            "this Node requires an initialized data epoch 5 directory"
        );
        let mut config = Config::load(dir)?;
        let process_lock = Arc::new(config::lock_data_dir(dir)?);
        let blob_staging = blob_transfer::BlobStaging::open(dir, process_lock.clone())?;
        let identity = config::identity(dir)?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.attach_process_lock(process_lock.clone());
        if store.has_network()? {
            let state = store.public_state()?;
            if state
                .project()?
                .authorize_node(&identity.peer_id()?, None)
                .is_ok()
            {
                store.save_local_identity(&state.genesis.network_id, &identity)?;
            }
        }
        let store = Arc::new(Mutex::new(store));
        let (hub, peer_rx) = PeerHub::new(identity.clone(), store.clone(), &config.roles);
        let (network, network_rx) = NetworkHub::new(identity.clone(), config.network.clone());
        let (events, _) = broadcast::channel(256);
        let (tx, rx) = mpsc::unbounded_channel();
        let manages_codex = config
            .manager
            .as_ref()
            .is_some_and(|manager| manager.provider == "codex_sdk");
        let managed_socket = manages_codex && config.codex_socket.is_none();
        if managed_socket {
            let socket_dir =
                PathBuf::from("/tmp").join(format!("vibeface-{}", &identity.peer_id()?[..32]));
            match std::fs::create_dir(&socket_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let metadata = std::fs::symlink_metadata(&socket_dir)?;
                ensure!(
                    metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == std::fs::metadata(dir)?.uid(),
                    "private socket directory has unexpected ownership"
                );
                std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;
            }
            config.codex_socket = Some(socket_dir.join("codex.sock"));
        }
        let mut codex_process = None;
        if managed_socket && config.roles.contains(&NodeRole::Scheduler) {
            let socket = config.codex_socket.as_ref().unwrap();
            let existing = tokio::net::UnixStream::connect(socket).await.is_ok();
            if !existing {
                if socket.exists() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::FileTypeExt;
                        ensure!(
                            std::fs::symlink_metadata(socket)?.file_type().is_socket(),
                            "private Codex socket path is occupied by another file"
                        );
                    }
                    std::fs::remove_file(socket)?;
                }
                match tokio::process::Command::new(&config.codex_bin)
                    .args(["app-server", "--listen"])
                    .arg(format!("unix://{}", socket.display()))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => codex_process = Some(child),
                    Err(error) => {
                        tracing::warn!(error=%error,"Codex not available; check the local management SDK configuration")
                    }
                }
            }
        }
        let codex = config
            .codex_socket
            .clone()
            .filter(|_| config.manager.is_some())
            .map(|socket| vibeface_bridge::codex::CodexClient::start(socket, tx));
        let local_access = Arc::new(crate::local_access::LocalAccess::open(
            dir,
            config.local_auto_access,
        )?);
        let node = Self {
            config,
            identity,
            store,
            network,
            hub,
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            manager_controls: Mutex::new(HashMap::new()),
            agent_blobs: Mutex::new(HashMap::new()),
            blob_staging,
            schedule_workers: Mutex::new(BTreeSet::new()),
            route_flights: Arc::new(Mutex::new(HashMap::new())),
            sync: Arc::new(SyncCoordinator::default()),
            schedule_wakeup: tokio::sync::Notify::new(),
            observation: std::sync::Mutex::new(Default::default()),
            tool_url: std::sync::OnceLock::new(),
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            codex,
            local_access,
            _codex_process: codex_process,
            _process_lock: process_lock,
            peer_rx: Mutex::new(Some(peer_rx)),
            network_rx: Mutex::new(Some(network_rx)),
            native_rx: Mutex::new(Some(rx)),
        };
        node.recover_blob_staging(MAX_BLOB_BYTES as u64).await;
        Ok(node)
    }
    pub async fn install_membership(&self, incoming: NetworkState) -> Result<AuthorizationView> {
        let _work = self.maintenance.enter()?;
        let mut store = self.store.lock().await;
        let mut state = incoming;
        if store.has_network()? {
            state = store.public_state()?.merge(&state)?;
        }
        let view = state.project()?;
        store.install_state(state)?;
        if view.authorize_node(&self.identity.peer_id()?, None).is_ok() {
            store.save_local_identity(&view.network_id, &self.identity)?;
        }
        Ok(view)
    }

    pub async fn has_membership(&self) -> Result<bool> {
        self.store.lock().await.has_network()
    }

    pub async fn membership_state(&self) -> Result<NetworkState> {
        self.store.lock().await.public_state()
    }
    pub fn info(&self) -> Value {
        json!({"peer_id":self.identity.peer_id().ok(),"version":env!("CARGO_PKG_VERSION"),"roles":self.config.roles,"manager_available":self.config.manager.is_some()})
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        self.start_schedule_delivery();
        self.start_observation_worker();
        self.start_native_observation();
        let mut receiver = self
            .peer_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                let n = node.clone();
                tokio::spawn(async move {
                    let response = n.handle_peer(&message).await;
                    let request_type = message.payload["type"].as_str();
                    let operation_id = message.payload["operation_id"]
                        .as_str()
                        .or_else(|| message.payload["request"]["operation_id"].as_str());
                    let trigger = message.payload["trigger"].as_str();
                    tracing::debug!(network=%message.network_id, origin=%message.origin,
                        request_id=%message.request_id, request_type, operation_id, trigger,
                        outcome=if response.is_ok() { "ok" } else { "error" },
                        "peer business request handled");
                    if !message.broadcast {
                        let value = response.unwrap_or_else(|e| json!({"error":format!("{e:#}")}));
                        let _ = n.hub.respond(&message, value).await;
                    }
                });
            }
        });
        let mut receiver = self
            .native_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if let Err(e) = node.observe_native(event).await {
                    tracing::warn!(error=%e,"could not persist native observation");
                }
            }
        });
        let mut incoming = self
            .network_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(incoming) = incoming.recv().await {
                let node = node.clone();
                tokio::spawn(async move {
                    let result = match incoming.service {
                        Service::Peer => node.hub.clone().accept_transport(incoming.socket).await,
                        Service::Client => {
                            crate::client::serve(node.clone(), incoming.socket).await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(%error,"authenticated channel closed");
                    }
                });
            }
        });
        let transport = self.network.clone();
        let listen = self.config.listen;
        tokio::spawn(async move {
            if let Err(error) = transport.run(listen).await {
                tracing::warn!(%error,"network discovery stopped");
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let Ok(state) = node.store.lock().await.public_state() else {
                    continue;
                };
                let Ok(view) = state.project() else { continue };
                let online = node.hub.online(&view.network_id).await;
                for member in view.members.into_iter().filter(|member| {
                    member.kind == IdentityKind::Node && !view.revoked.contains(&member.subject_id)
                }) {
                    if member.subject_id == node.identity.peer_id().unwrap_or_default()
                        || online.contains(&member.subject_id)
                    {
                        continue;
                    }
                    if let Err(error) = node
                        .route_flight(
                            &view.network_id,
                            &member.subject_id,
                            "background",
                            None,
                            None,
                        )
                        .await
                    {
                        tracing::debug!(%error, "personal peer reconnect pending");
                    }
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                node.recover_blob_staging(MAX_BLOB_BYTES as u64).await;
                let networks = node.store.lock().await.networks().unwrap_or_default();
                for network in networks {
                    let _ = node.enqueue_sync_all(&network).await;
                }
            }
        });
        Ok(())
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        let store = self.store.lock().await;
        for network in store.networks()? {
            crate::scheduler::maintenance_idle(&store, &network)?;
            ensure!(
                native_observation::sessions_from(&store, &network)?
                    .iter()
                    .all(|work| work.state.terminal()),
                Busy("native work still has an unresolved outcome".into())
            );
        }
        Ok(())
    }
    pub async fn authorize_bearer(&self, token: &str) -> Option<Access> {
        self.agent_tokens.lock().await.get(token).cloned()
    }
    pub async fn append(&self, network: &str, scope: &str, body: EventBody) -> Result<SignedEvent> {
        self.append_blobs(network, scope, body, vec![]).await
    }
    pub async fn append_blobs(
        &self,
        network: &str,
        scope: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
    ) -> Result<SignedEvent> {
        let current = self.membership_state().await?;
        ensure!(
            current.genesis.network_id == network,
            "wrong personal network"
        );
        let event = self.store.lock().await.append_event(
            &self.identity,
            network,
            scope,
            body,
            blobs,
            now_ms(),
        )?;
        self.publish_changed(network, scope, &event.hash);
        Ok(event)
    }
    fn publish_changed(&self, network: &str, scope: &str, event_hash: &str) {
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload: json!({"type":"changed","conversation_id":scope,"event_hash":event_hash}),
        });
    }
    pub async fn raise_notification(
        &self,
        network: &str,
        scope: &str,
        kind: &str,
        text: &str,
    ) -> Result<()> {
        self.append(
            network,
            scope,
            EventBody::NotificationRaised {
                notification_id: uuid::Uuid::new_v4().to_string(),
                kind: kind.into(),
                text: text.chars().take(8192).collect(),
            },
        )
        .await?;
        Ok(())
    }
    async fn require_scope(&self, access: &Access, scope: &str) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == access.network_id,
            "wrong personal network"
        );
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == scope, "outside this tool's conversation");
        }
        let waiting = Instant::now();
        let store = self.store.lock().await;
        let waited = waiting.elapsed();
        let held = Instant::now();
        store.authorize_node(&access.network_id, &self.identity.peer_id()?, None)?;
        ensure!(
            store.has_conversation(&access.network_id, scope)?,
            "conversation not found"
        );
        tracing::trace!(network=%access.network_id, scope, stage="scope_access", wait_ms=waited.as_millis() as u64,
            hold_ms=held.elapsed().as_millis() as u64, "history store phase finished");
        Ok(())
    }
    async fn require_role(&self, network: &str, role: NodeRole) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == network,
            "wrong personal network"
        );
        ensure!(
            self.config.roles.contains(&role),
            "node capability is disabled"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, &self.identity.peer_id()?, Some(role))?;
        Ok(())
    }
    async fn route_flight(
        &self,
        network: &str,
        target: &str,
        trigger: &'static str,
        request_type: Option<&str>,
        operation_id: Option<&str>,
    ) -> Result<watch::Receiver<RouteFlightState>> {
        let key = (network.to_owned(), target.to_owned());
        let mut flights = self.route_flights.lock().await;
        if let Some(existing) = flights.get(&key) {
            let state = existing.state.borrow().clone();
            let current = match &state {
                RouteFlightState::Ready(id) => self.hub.link_matches(network, target, id).await,
                _ => true,
            };
            if current {
                tracing::debug!(network, target, flight_id=%existing.id, trigger, request_type, operation_id,
                    state=?state, "peer route flight joined");
                return Ok(existing.state.subscribe());
            }
            flights.remove(&key);
        }
        ensure!(flights.len() < MAX_ROUTE_FLIGHTS, RequestFailure::Busy);
        let id = uuid::Uuid::new_v4().to_string();
        let deadline = tokio::time::Instant::now() + crate::network::DIAL_TIMEOUT;
        let (state, receive) = watch::channel(RouteFlightState::Connecting);
        flights.insert(
            key.clone(),
            RouteFlight {
                id: id.clone(),
                state: state.clone(),
            },
        );
        tracing::debug!(network, target, flight_id=%id, trigger, request_type, operation_id, "peer route flight started");
        let hub = self.hub.clone();
        let transport = self.network.clone();
        let registry = self.route_flights.clone();
        tokio::spawn(async move {
            let (ready, receive_ready) = oneshot::channel();
            let connection = hub.connect_target(
                key.0.clone(),
                key.1.clone(),
                transport,
                Some(ready),
                deadline,
            );
            tokio::pin!(connection);
            let result = tokio::select! {
                ready = receive_ready => {
                    if let Ok(link_id) = ready {
                        state.send_replace(RouteFlightState::Ready(link_id.clone()));
                        tracing::debug!(network=%key.0, target=%key.1, flight_id=%id, link_id,
                            state="ready", "peer route flight ready");
                    }
                    connection.await
                }
                result = &mut connection => result,
            };
            let failure = match result {
                Err(error) => RouteFailure::from_error(&error),
                Ok(()) => RouteFailure::Unavailable("peer connection closed".to_owned()),
            };
            tracing::debug!(network=%key.0, target=%key.1, flight_id=%id,
                state=?failure, "peer route flight finished");
            state.send_replace(RouteFlightState::Failed(failure));
            let mut flights = registry.lock().await;
            if flights.get(&key).is_some_and(|entry| entry.id == id) {
                flights.remove(&key);
            }
        });
        Ok(receive)
    }

    async fn raw_peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let local = self.identity.peer_id()?;
        self.hub.authorize(network, &local).await?;
        self.hub.authorize(network, target).await?;
        if target != local && !self.hub.has_route(network, target).await {
            let request_type = payload["type"].as_str();
            let operation_id = payload["operation_id"]
                .as_str()
                .or_else(|| payload["request"]["operation_id"].as_str());
            let mut receive = self
                .route_flight(network, target, "on_demand", request_type, operation_id)
                .await?;
            loop {
                let state = receive.borrow().clone();
                match state {
                    RouteFlightState::Connecting => receive
                        .changed()
                        .await
                        .map_err(|_| RequestFailure::NoRoute)?,
                    RouteFlightState::Ready(_) => break,
                    RouteFlightState::Failed(RouteFailure::Denied(error)) => {
                        return Err(error.into());
                    }
                    RouteFlightState::Failed(RouteFailure::Busy) => {
                        return Err(RequestFailure::Busy.into());
                    }
                    RouteFlightState::Failed(RouteFailure::Unavailable(error)) => {
                        tracing::debug!(network, target, %error, "peer route unavailable before first send");
                        break;
                    }
                }
            }
        }
        self.hub.authorize(network, &local).await?;
        self.hub.authorize(network, target).await?;
        self.hub.request(network, target, payload).await
    }

    async fn peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let reply = self.raw_peer_request(network, target, payload).await?;
        if let Some(error) = reply["error"].as_str() {
            bail!("{error}");
        }
        Ok(reply)
    }
    async fn read_peer(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.peer_request(network, target, payload),
        )
        .await
        .map_err(|_| RequestFailure::Unknown)?
    }
    async fn history_response(
        &self,
        network: &str,
        target: &str,
        payload: Value,
        budget: &mut HistoryBudget,
        class: ReadClass,
    ) -> Result<HistoryReply> {
        let reply = self
            .read_history(
                network,
                target,
                payload,
                class,
                crate::peer::MAX_PACKET_BYTES,
            )
            .await?;
        budget.response(&reply.value)?;
        if let Some(error) = reply.value["error"].as_str() {
            bail!("{error}");
        }
        Ok(reply)
    }
    async fn history_sources(
        &self,
        network: &str,
        preferred: &[String],
    ) -> Result<Vec<HistorySource>> {
        let local = self.identity.peer_id()?;
        let mut sources = preferred.to_vec();
        sources.extend(self.hub.online(network).await);
        let mut seen = BTreeSet::new();
        let store = self.store.lock().await;
        sources.retain(|source| {
            source != &local
                && seen.insert(source.clone())
                && store
                    .authorize_node(network, source, Some(NodeRole::Replica))
                    .is_ok()
        });
        Ok(sources
            .into_iter()
            .map(|peer_id| HistorySource {
                peer_id,
                batch: None,
                failed: false,
            })
            .collect())
    }
    async fn history_events(
        &self,
        network: &str,
        sources: &mut [HistorySource],
        hash: &str,
        budget: &mut HistoryBudget,
        class: ReadClass,
    ) -> Result<HistoryPage> {
        let initial_events = budget.events;
        let mut busy = false;
        for index in 0..sources.len() {
            let source = &mut sources[index];
            if source.failed {
                continue;
            }
            if budget.events == 0 || budget.bytes == 0 {
                break;
            }
            let target = source.peer_id.clone();
            let before = budget.events;
            let result = async {
                let batch = match source.batch {
                    Some(batch) => batch,
                    None => {
                        let capabilities = self.history_response(network, &target, json!({"type":"capabilities"}), budget,class).await?;
                        let batch = capabilities.value["history_batch"]["version"] == 1;
                        source.batch = Some(batch);
                        batch
                    }
                };
                let limit = budget.events.min(HISTORY_BATCH_EVENTS);
                let max_bytes = budget.bytes.min(HISTORY_BATCH_BYTES);
                let value = if batch {
                    self.history_response(network, &target, json!({"type":"history_batch","hash":hash,"limit":limit,"max_bytes":max_bytes}), budget,class).await?
                } else {
                    self.history_response(network,&target,json!({"type":"event","hash":hash}),budget,class).await?
                };
                let single = !batch || (value.value["version"] == 1 && value.value["hash"] == hash && value.value["single_event"] == true && value.value.get("events").is_none() && serde_json::to_vec(&value.value)?.len() <= max_bytes);
                let value = if single && batch {
                    drop(value);
                    self.history_response(network,&target,json!({"type":"event","hash":hash}),budget,class).await?
                } else { value };
                budget.events = budget.events.saturating_sub(if single {1} else {value.value["events"].as_array().map_or(1,Vec::len).clamp(1,limit)});
                let store = self.store.lock().await;
                store.authorize_node(network, &target, Some(NodeRole::Replica))?;
                let envelopes = if single {
                    let envelope: SignedEvent = serde_json::from_value(value.value["event"].clone())?;
                    ensure!(envelope.hash == hash, "event response hash mismatch");
                    store.verify_event(network, &envelope)?;
                    vec![envelope]
                } else {
                    verify_history_batch(&store, network, hash, &value.value, limit, max_bytes)?
                };
                if !single {
                    tracing::trace!(network, target=%target, hash, count=envelopes.len(), "history batch fetched");
                }
                for envelope in &envelopes {
                    tracing::trace!(network, target=%target, hash=%envelope.hash, mode=if single {"single"} else {"batch"}, "history event fetched");
                }
                Ok(HistoryPage { events:envelopes, _reply:Some(value) })
            }
            .await;
            match result {
                Ok(envelopes) => {
                    let length = sources.len();
                    sources.rotate_left((index + 1) % length);
                    return Ok(envelopes);
                }
                Err(error) => {
                    if error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy) {
                        busy = true;
                    } else {
                        source.failed = true;
                        if budget.events == before {
                            budget.events -= 1;
                        }
                    }
                    tracing::debug!(network,target=%target, error=%error, "history object source unavailable")
                }
            }
        }
        if busy {
            return Err(RequestFailure::Busy.into());
        }
        if budget.events == initial_events && budget.events != 0 {
            budget.events -= 1;
        }
        bail!("event unavailable from authorized replicas")
    }
    async fn sync_step_core(
        self: &Arc<Self>,
        spec: &sync::SyncSpec,
        source_cursor: usize,
    ) -> Result<(ClosureSync, bool)> {
        let network = spec.network.as_str();
        let scope = spec.scope.as_str();
        let target = spec
            .sources
            .first()
            .context("history unavailable from authorized replicas")?;
        let wanted = &spec.roots;
        let budget = spec.event_budget;
        let class = spec.class;
        let persist = class == ReadClass::Background;
        self.membership_state().await?;
        ensure!(
            !scope.is_empty() && scope.len() <= 512,
            "invalid history scope"
        );
        let mut roots = wanted.to_vec();
        roots.sort();
        roots.dedup();
        for hash in &roots {
            ensure!(hex::decode(hash)?.len() == 32, "invalid history hash");
        }
        let key = format!("net1:v1:closure:{}", crypto::digest(scope.as_bytes()));
        let mut sources = self.history_sources(network, &spec.sources).await?;
        if !sources.is_empty() {
            let length = sources.len();
            sources.rotate_left(source_cursor % length);
        }
        let waiting = Instant::now();
        let (mut progress, generation) = {
            let store = self.store.lock().await;
            let waited = waiting.elapsed();
            let held = Instant::now();
            let generation = store.projection_generation(network)?;
            if persist {
                let mut combined = store.heads(network, scope)?;
                for hash in &roots {
                    if let Some(envelope) = store.event(network, hash)? {
                        ensure!(
                            store.verify_event(network, &envelope)?.scope_id == scope,
                            "history head scope mismatch"
                        );
                    } else {
                        combined.push(hash.clone());
                    }
                }
                combined.sort();
                combined.dedup();
                roots = combined;
            }
            let previous = if persist {
                store.local_value(network, &key)?
            } else {
                None
            };
            let previous = previous
                .map(serde_json::from_value::<SyncProgress>)
                .transpose()?;
            let progress = if let Some(mut progress) = previous {
                if progress.heads != roots {
                    progress.heads = roots.clone();
                    progress.frontier_generation = None;
                    progress.advance(&store.missing_dependencies(network, &roots)?);
                }
                progress
            } else {
                SyncProgress::new(
                    scope,
                    roots.clone(),
                    &store.missing_dependencies(network, &roots)?,
                )
            };
            ensure!(
                progress.version == 1
                    && progress.scope == scope
                    && progress.heads == roots
                    && progress.events.len() <= 1024
                    && progress.blobs.len() <= 128,
                "invalid history continuation"
            );
            tracing::trace!(
                network,
                scope,
                stage = "scope_roots",
                wait_ms = waited.as_millis() as u64,
                hold_ms = held.elapsed().as_millis() as u64,
                "history store phase finished"
            );
            (progress, generation)
        };
        let before = progress.clone();
        let invalidated = progress
            .frontier_generation
            .is_some_and(|frontier| frontier != generation);
        if invalidated {
            progress.frontier_generation = None;
        }
        save_sync_progress(
            &mut *self.store.lock().await,
            network,
            &key,
            &progress,
            persist,
        )?;
        let mut pending: VecDeque<String> = progress.events.iter().cloned().collect();
        let mut queued: BTreeSet<String> = progress.events.iter().cloned().collect();
        if invalidated {
            for root in &roots {
                if queued.insert(root.clone()) {
                    pending.push_back(root.clone());
                }
            }
        }
        let mut attempted = BTreeSet::new();
        let mut unavailable = BTreeSet::new();
        let mut inserted = 0;
        let mut blobs: BTreeSet<BlobRef> = progress.blobs.iter().cloned().collect();
        let mut errors = Vec::new();
        let mut deferred = false;
        let mut work = HistoryBudget {
            events: budget,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        while work.events > 0 && work.bytes >= MAX_EVENT_BYTES {
            let Some(hash) = pending.pop_front() else {
                break;
            };
            if !attempted.insert(hash.clone()) {
                continue;
            }
            queued.remove(&hash);
            progress.event_cursor = Some(hash.clone());
            let local = self.store.lock().await.event(network, &hash)?;
            let page = match local {
                Some(envelope) => {
                    work.events -= 1;
                    work.bytes = work.bytes.saturating_sub(envelope.canonical_body.len());
                    HistoryPage {
                        events: vec![envelope],
                        _reply: None,
                    }
                }
                None => match self
                    .history_events(network, &mut sources, &hash, &mut work, class)
                    .await
                {
                    Ok(envelopes) => envelopes,
                    Err(error) => {
                        deferred |=
                            error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy);
                        errors.push(error.to_string());
                        unavailable.insert(hash);
                        continue;
                    }
                },
            };
            ensure!(
                page.first().is_some_and(|envelope| envelope.hash == hash),
                "stored event hash mismatch"
            );
            for envelope in &page.events {
                let hash = &envelope.hash;
                attempted.insert(hash.clone());
                queued.remove(hash);
                progress.event_cursor = Some(hash.clone());
                let event = self.store.lock().await.verify_event(network, envelope)?;
                ensure!(
                    roots.binary_search(hash).is_err() || event.scope_id == scope,
                    "history head scope mismatch"
                );
                {
                    let store = self.store.lock().await;
                    for parent in event.parents.iter().chain(event.previous.iter()) {
                        if !attempted.contains(parent)
                            && store.event(network, parent)?.is_none()
                            && queued.insert(parent.clone())
                        {
                            pending.push_back(parent.clone());
                        }
                    }
                    for blob in &event.blobs {
                        if !store.has_blob(network, blob)? {
                            blobs.insert(blob.clone());
                        }
                    }
                }
            }
            {
                let mut store = self.store.lock().await;
                store.authorize_node(
                    network,
                    &self.identity.peer_id()?,
                    Some(NodeRole::Replica),
                )?;
                inserted += store.ingest_events(network, &page.events)?;
            }
            drop(page);
        }
        queued.extend(unavailable);
        let mut missing = MissingDependencies {
            events: queued.into_iter().collect(),
            blobs: blobs.iter().cloned().collect(),
            conflicts: Vec::new(),
        };
        {
            let mut store = self.store.lock().await;
            store.authorize_node(network, &self.identity.peer_id()?, Some(NodeRole::Replica))?;
            progress.advance(&missing);
            save_sync_progress(&mut store, network, &key, &progress, persist)?;
        }
        let blob_page = rotated_page(
            &blobs.into_iter().collect::<Vec<_>>(),
            progress.blob_cursor.as_ref(),
            4,
        );
        let mut blobs_completed = 0;
        let mut blob_quantum = blob_transfer::BlobQuantum::new(4);
        for blob in blob_page {
            progress.blob_cursor = Some(blob.clone());
            if self.store.lock().await.has_blob(network, &blob)? {
                missing.blobs.retain(|wanted| wanted != &blob);
                continue;
            }
            match self
                .history_blob(network, &sources, &blob, class, &mut blob_quantum)
                .await
            {
                Ok(blob_transfer::BlobStep::Complete { changed }) => {
                    missing.blobs.retain(|wanted| wanted != &blob);
                    blobs_completed += usize::from(changed);
                }
                Ok(blob_transfer::BlobStep::Partial) => {}
                Err(error) => {
                    deferred |=
                        error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy);
                    errors.push(error.to_string());
                }
            }
            if blob_quantum.remaining == 0 {
                break;
            }
        }
        let waiting = Instant::now();
        let changed = {
            let mut store = self.store.lock().await;
            let waited = waiting.elapsed();
            let held = Instant::now();
            for hash in &roots {
                let verified = store.event(network, hash).and_then(|envelope| {
                    if let Some(envelope) = envelope {
                        ensure!(
                            store.verify_event(network, &envelope)?.scope_id == scope,
                            "history head scope mismatch"
                        );
                    }
                    Ok(())
                });
                if let Err(error) = verified {
                    progress.frontier_generation = None;
                    save_sync_progress(&mut store, network, &key, &progress, persist)?;
                    return Err(error);
                }
            }
            if missing.is_complete() || (inserted == 0 && blobs_completed == 0) {
                missing = match store.missing_dependencies(network, &roots) {
                    Ok(missing) => missing,
                    Err(error) => {
                        progress.frontier_generation = None;
                        save_sync_progress(&mut store, network, &key, &progress, persist)?;
                        return Err(error);
                    }
                };
            }
            let after_generation = store.projection_generation(network)?;
            progress.advance(&missing);
            progress.frontier_generation = missing.is_complete().then_some(after_generation);
            save_sync_progress(&mut store, network, &key, &progress, persist)?;
            let completeness_changed = persist
                && (before.frontier_generation.is_some() != progress.frontier_generation.is_some());
            tracing::trace!(
                network,
                scope,
                stage = "scope_frontier",
                wait_ms = waited.as_millis() as u64,
                hold_ms = held.elapsed().as_millis() as u64,
                "history store phase finished"
            );
            inserted != 0 || blobs_completed != 0 || completeness_changed
        };
        let processed = budget - work.events;
        let state = if missing.is_complete() {
            SyncState::Complete
        } else if !missing.conflicts.is_empty() || deferred {
            SyncState::Partial
        } else if sources.is_empty() || (processed > 0 && !errors.is_empty() && inserted == 0) {
            SyncState::Unreachable
        } else {
            SyncState::Partial
        };
        if changed {
            let _ = self.events.send(Update {
                network_id: network.into(),
                payload: json!({"type":"changed","conversation_id":scope}),
            });
        }
        Ok((
            ClosureSync {
                scope_id: scope.into(),
                target: target.into(),
                state,
                missing,
                processed,
                changed,
                errors,
            },
            inserted != 0 || blob_quantum.remaining != 4 || deferred,
        ))
    }
    fn capabilities(&self) -> Value {
        let manager =
            self.config
                .manager
                .as_ref()
                .and_then(|settings| match settings.provider.as_str() {
                    "cursor_sdk" => Some(crate::manager::ManagerKind::CursorSdk),
                    "codex_sdk" => Some(crate::manager::ManagerKind::CodexSdk),
                    _ => None,
                });
        json!({"roots":self.config.roots,
            "manager":self.config.manager.as_ref().map(|m|m.provider.clone()),
            "manager_capabilities":manager.map(|kind|kind.capabilities()),"reliable_schedule":{"version":1},
            "history_batch":history_batch_capability()})
    }

    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let write = !matches!(
            input["method"].as_str(),
            Some("nodes" | "conversation" | "conversations" | "bots" | "notifications" | "export")
        );
        let task = input["task_id"]
            .as_str()
            .or_else(|| input["request"]["task_id"].as_str());
        let _lease = self.manager_tool_lease(&access, write, task).await?;
        self.rpc_inner(access, input).await
    }
    async fn rpc_inner(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let current = self.membership_state().await?;
        let network = &access.network_id;
        ensure!(
            current.genesis.network_id == *network,
            "wrong personal network"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, &self.identity.peer_id()?, None)?;
        let method = field(&input, "method")?;
        if !access.human {
            ensure!(
                matches!(method, "nodes" | "create_task" | "update_task"),
                "method unavailable to Agent tokens"
            );
        }
        let scope = input["conversation_id"].as_str().map(String::from);
        if let Some(scope) = &scope
            && !matches!(method, "create_conversation" | "create_bot")
        {
            self.require_scope(&access, scope).await?;
        }
        match method {
            "nodes" => {
                let bundle = self
                    .store
                    .lock()
                    .await
                    .trust_bundle(network)?
                    .context("unknown network")?;
                let online = self.hub.online(network).await;
                let local = self.identity.peer_id()?;
                let mut nodes = vec![];
                let view = bundle.project()?;
                let revoked_ids = view.revoked.clone();
                let display_names = node_display_names(&bundle);
                for member in view
                    .members
                    .into_iter()
                    .filter(|member| member.kind == IdentityKind::Node)
                {
                    let mut cert = member.certificate()?;
                    cert.revoked = view.revoked.contains(&cert.peer_id);
                    let here = cert.peer_id == local;
                    let live = here || online.contains(&cert.peer_id);
                    let capabilities = if here {
                        self.capabilities()
                    } else if live && !cert.revoked {
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            self.peer_request(
                                network,
                                &cert.peer_id,
                                json!({"type":"capabilities"}),
                            ),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    };
                    nodes.push(json!({"peer_id":cert.peer_id,"name":member.name,"roles":cert.roles,"revoked":cert.revoked,"local":here,"online":live&&!cert.revoked,"capabilities":capabilities}));
                }
                let history = bundle.historical_nodes()?;
                for revoked in revoked_ids {
                    if nodes.iter().any(|node| node["peer_id"] == revoked) {
                        continue;
                    }
                    if history.iter().any(|node| node.subject_id == revoked) {
                        let name = display_names.get(&revoked).cloned().unwrap_or_default();
                        nodes.push(json!({"peer_id":revoked,"name":name,"roles":[],"revoked":true,"local":false,"online":false,"capabilities":Value::Null}));
                    }
                }
                Ok(json!({"nodes":nodes}))
            }
            "conversations" => {
                Ok(json!({"conversations":self.store.lock().await.conversations(network)?}))
            }
            "create_conversation" => {
                ensure!(access.human, "only the user creates conversations");
                let scope = id(&input, "conversation_id")?;
                let title = field(&input, "title")?;
                ensure!(title.len() <= 512, "title too long");
                {
                    let store = self.store.lock().await;
                    if let Some(old) = store
                        .conversations(network)?
                        .into_iter()
                        .find(|c| c.conversation_id == scope)
                    {
                        ensure!(old.title == title, "conversation ID already exists");
                        return Ok(json!(old));
                    }
                }
                self.append(
                    network,
                    &scope,
                    EventBody::ConversationCreated {
                        title: title.into(),
                    },
                )
                .await?;
                Ok(
                    json!({"conversation_id":scope,"title":title,"heads":self.store.lock().await.heads(network,&scope)?}),
                )
            }
            "conversation" => self.conversation_snapshot(&access, &input).await,
            "message" => self.accept_chat_message(&access, &input).await,
            "set_followup" => {
                let scope = scope.context("conversation_id is required")?;
                let result = self.set_followup(&access, &scope, &input).await?;
                self.schedule_wakeup.notify_one();
                Ok(result)
            }
            "read_artifact" => self.read_artifact(&access, &input).await,
            "cancel_agent_task" => self.cancel_task_rpc(&access, &input).await,
            "native_interrupt" | "native_respond" => self.native_control(&access, &input).await,
            "bots" => self.bots(&access).await,
            "create_bot" => self.create_bot(&access, &input).await,
            "cancel_manager" => self.cancel_manager(&access, &input).await,
            "resolve_manager_unknown" => self.resolve_manager_invocation(&access, &input).await,
            "create_task" => {
                ensure!(
                    input["auto_followup"].as_bool() != Some(true),
                    "use the task follow-up control to grant permission"
                );
                let scope = scope.context("conversation_id is required")?;
                let body = EventBody::TaskCreated {
                    task_id: id(&input, "task_id")?,
                    title: field(&input, "title")?.into(),
                    goal: field(&input, "goal")?.into(),
                    acceptance: field(&input, "acceptance")?.into(),
                };
                let origin = access
                    .manager
                    .as_ref()
                    .map(|m| (m.trigger.as_str(), m.attempt_id.as_str()));
                let event = {
                    let mut store = self.store.lock().await;
                    Self::check_manager_permission(&store, &access, true, None)?;
                    store.create_task_with_origin(
                        &self.identity,
                        network,
                        &scope,
                        body,
                        origin,
                        now_ms(),
                    )?
                };
                self.publish_changed(network, &scope, &event.hash);
                Ok(json!(event))
            }
            "update_task" => {
                let scope = scope.context("conversation_id is required")?;
                let task_id = id(&input, "task_id")?;
                let status = field(&input, "status")?;
                ensure!(
                    matches!(
                        status,
                        "open" | "working" | "blocked" | "needs_review" | "accepted" | "cancelled"
                    ),
                    "invalid task status"
                );
                ensure!(
                    access.human || !matches!(status, "accepted" | "cancelled"),
                    "only the user accepts or cancels tasks"
                );
                let event = {
                    let mut store = self.store.lock().await;
                    Self::check_manager_permission(&store, &access, true, Some(&task_id))?;
                    require_task_write(&store, network, &scope, &task_id, access.human)?;
                    store.append_event(
                        &self.identity,
                        network,
                        &scope,
                        EventBody::TaskChanged {
                            task_id,
                            status: status.into(),
                            detail: input["detail"]
                                .as_str()
                                .unwrap_or("")
                                .chars()
                                .take(65536)
                                .collect(),
                        },
                        vec![],
                        now_ms(),
                    )?
                };
                self.publish_changed(network, &scope, &event.hash);
                Ok(json!(event))
            }
            "notifications" => Ok(
                json!({"notifications":self.store.lock().await.pending_notifications(network,field(&input,"client_id")?)?}),
            ),
            "ack" => {
                ensure!(access.human, "only clients acknowledge notifications");
                let notification = id(&input, "notification_id")?;
                let client = id(&input, "client_id")?;
                let pending = self
                    .store
                    .lock()
                    .await
                    .pending_notifications(network, &client)?;
                if let Some(n) = pending
                    .into_iter()
                    .find(|n| n.notification_id == notification)
                {
                    self.append(
                        network,
                        &n.conversation_id,
                        EventBody::NotificationAcknowledged {
                            notification_id: notification,
                            client_id: client,
                        },
                    )
                    .await?;
                }
                Ok(json!({"acknowledged":true}))
            }
            "export" => {
                let scope = scope.context("conversation_id is required")?;
                Ok(json!(
                    self.store.lock().await.export_snapshot(network, &scope)?
                ))
            }
            _ => bail!("unknown method"),
        }
    }
    pub async fn tool(
        self: &Arc<Self>,
        access: Access,
        name: &str,
        args: Value,
        call_id: &str,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(!access.human, "tool endpoint requires a scoped Agent token");
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        self.require_scope(&access, scope).await?;
        self.agent_tool(access, name, args, call_id).await
    }
    fn allowed_path(&self, path: &str) -> Result<PathBuf> {
        let path = std::fs::canonicalize(path).context("execution path does not exist")?;
        ensure!(
            self.config
                .roots
                .iter()
                .filter_map(|r| std::fs::canonicalize(r).ok())
                .any(|r| path.starts_with(r)),
            "execution path is outside configured roots"
        );
        ensure!(path.is_dir(), "execution path must be a directory");
        Ok(path)
    }
    pub async fn current_facts(&self, network: &str, scope: &str) -> Result<Value> {
        Ok(
            json!({"agent_tasks":self.agent_task_projections(network, scope).await?,
            "native_sessions":self.native_session_projections(network, scope).await?,"freshness":"locally_known"}),
        )
    }
    async fn handle_peer(self: &Arc<Self>, message: &Inbound) -> Result<Value> {
        self.membership_state().await?;
        let network = &message.network_id;
        let input = &message.payload;
        self.store
            .lock()
            .await
            .authorize_node(network, &message.origin, None)?;
        let kind = field(input, "type")?;
        let _work = (!matches!(
            kind,
            "capabilities"
                | "inventory"
                | "event"
                | "history_batch"
                | "blob"
                | "progress_read"
                | "native_status"
        ))
        .then(|| self.maintenance.enter())
        .transpose()?;
        match kind {
            "capabilities" => Ok(self.capabilities()),
            "inventory" => {
                self.require_role(network, NodeRole::Replica).await?;
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Replica),
                )?;
                let store = self.store.lock().await;
                let scopes = store
                    .inventory_heads(network)?
                    .into_iter()
                    .map(|(scope, heads)| json!({"scope_id":scope,"heads":heads}))
                    .collect::<Vec<_>>();
                Ok(
                    json!({"membership":store.trust_bundle(network)?,"scopes":scopes,"history_batch":history_batch_capability()}),
                )
            }
            "event" => {
                self.require_role(network, NodeRole::Replica).await?;
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Replica),
                )?;
                Ok(json!({"event":self.store.lock().await.event(network,field(input,"hash")?)?}))
            }
            "history_batch" => {
                self.require_role(network, NodeRole::Replica).await?;
                let store = self.store.lock().await;
                store.authorize_node(network, &message.origin, Some(NodeRole::Replica))?;
                let hash = field(input, "hash")?;
                ensure!(hex::decode(hash)?.len() == 32, "invalid history hash");
                let limit = input["limit"]
                    .as_u64()
                    .unwrap_or(HISTORY_BATCH_EVENTS as u64)
                    .clamp(1, HISTORY_BATCH_EVENTS as u64) as usize;
                let max_bytes = input["max_bytes"]
                    .as_u64()
                    .unwrap_or(HISTORY_BATCH_BYTES as u64)
                    .min(HISTORY_BATCH_BYTES as u64) as usize;
                let mut value = json!({"version":1,"hash":hash,"events":[]});
                let mut bytes = serde_json::to_vec(&value)?.len();
                ensure!(max_bytes >= bytes, "history batch byte budget too small");
                let mut pending = VecDeque::from([hash.to_owned()]);
                let mut queued = BTreeSet::from([hash.to_owned()]);
                let mut examined = 0;
                while let Some(next) = pending.pop_front() {
                    examined += 1;
                    let Some(envelope) = store.event(network, &next)? else {
                        ensure!(next != hash, "history event unavailable");
                        continue;
                    };
                    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
                    let encoded = serde_json::to_value(&envelope)?;
                    let events = value["events"].as_array_mut().unwrap();
                    let added =
                        serde_json::to_vec(&encoded)?.len() + usize::from(!events.is_empty());
                    if bytes + added > max_bytes {
                        if events.is_empty() {
                            let marker = json!({"version":1,"hash":hash,"single_event":true});
                            ensure!(
                                serde_json::to_vec(&marker)?.len() <= max_bytes,
                                "history batch byte budget too small"
                            );
                            return Ok(marker);
                        }
                        break;
                    }
                    bytes += added;
                    events.push(encoded);
                    for parent in event.parents.into_iter().chain(event.previous) {
                        if queued.len() == limit {
                            break;
                        }
                        if queued.insert(parent.clone()) {
                            pending.push_back(parent);
                        }
                    }
                    if examined == limit {
                        break;
                    }
                }
                ensure!(
                    serde_json::to_vec(&value)?.len() <= max_bytes,
                    "history batch byte limit exceeded"
                );
                Ok(value)
            }
            "blob" => {
                self.require_role(network, NodeRole::Replica).await?;
                let offset = input["offset"].as_u64().context("offset required")?;
                let limit = input["limit"].as_u64().unwrap_or(65536).clamp(1, 65536) as usize;
                let mut store = self.store.lock().await;
                let view = store.public_state()?.project()?;
                ensure!(view.network_id == *network, "wrong personal network");
                view.authorize_node(&self.identity.peer_id()?, Some(NodeRole::Replica))?;
                let (size, bytes) = store
                    .blob_range(
                        network,
                        &self.identity.peer_id()?,
                        &message.origin,
                        field(input, "hash")?,
                        offset,
                        limit,
                    )?
                    .context("blob unavailable")?;
                tracing::trace!(network, origin=%message.origin, request_id=%message.request_id, hash=%field(input,"hash")?, offset,
                    returned_bytes=bytes.len(), size, "peer blob range served");
                Ok(json!({"data":hex::encode(bytes),"size":size}))
            }
            "trust" => {
                let state: NetworkState = serde_json::from_value(input["state"].clone())?;
                ensure!(
                    state.genesis.network_id == *network,
                    "wrong membership network"
                );
                self.install_membership(state).await?;
                Ok(json!({"received":true}))
            }
            "schedule" => self.receive_schedule(message).await,
            "manager_status" => {
                self.manager_status(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "trigger")?,
                )
                .await
            }
            "progress_read" => {
                ensure!(
                    !message.broadcast,
                    "progress observation requires a direct request"
                );
                self.read_progress(network, &message.origin, input).await
            }
            "native_status" => {
                self.native_status(network, field(input, "conversation_id")?)
                    .await
            }
            "cancel_agent_task" => {
                self.cancel_agent_task(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "task_ref")?,
                    &message.origin,
                )
                .await
            }
            "native_interrupt" | "native_respond" => {
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                ensure!(input["human"] == true, "native controls require the user");
                self.native_control_local(network, input, kind).await
            }
            "cancel_manager" | "resolve_manager_unknown" => {
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                ensure!(
                    input["human"] == true,
                    "management control requires the user"
                );
                let access = Access {
                    network_id: network.clone(),
                    human: true,
                    conversation_id: Some(field(input, "conversation_id")?.into()),
                    manager: None,
                };
                if kind == "cancel_manager" {
                    self.cancel_manager(&access, input).await
                } else {
                    self.resolve_manager_invocation(&access, input).await
                }
            }
            _ => bail!("unknown peer message"),
        }
    }
}
