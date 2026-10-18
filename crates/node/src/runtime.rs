use crate::{
    config::{self, Config},
    crypto::{self, LocalIdentity},
    engine::{Engine, EngineAction, ExecutorConfig, ExecutorKind, ExecutorSession},
    identity::{AuthorizationView, IdentityKind, IdentityStore, NetworkState},
    maintenance::{Busy, Gate},
    model::*,
    network::{IncomingSocket, NetworkHub, Service},
    peer::{Inbound, PeerHub},
    store::Store,
};
use anyhow::{Context as _, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast, mpsc};

mod agent_tools;
mod blob_transfer;
mod bots;
mod chat_controls;
mod commands;
mod execution_subscription;
mod followup;
mod manager_control;
#[cfg(test)]
mod manager_control_tests;
mod observation;
mod schedule_delivery;
pub use manager_control::ManagerAccess;
pub(crate) use observation::ObservationGuard;

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
                format!("runtime-test-root:{}", node.identity.node_id()?).as_bytes(),
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
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":3}"#)?;
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
        let a = nodes[1].identity.node_id()?;
        let b = nodes[2].identity.node_id()?;
        let sources = vec![
            HistorySource {
                node_id: a.clone(),
                batch: None,
            },
            HistorySource {
                node_id: b.clone(),
                batch: None,
            },
        ];
        let error = nodes[0]
            .history_blob(&network, &sources, &blob)
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
        assert!(nodes[0].history_blob(&network, &sources, &blob).await?);
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
        assert!(nodes[0].history_blob(&network, &sources, &empty).await?);
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
        assert!(nodes[0].store.lock().await.operations(&network)?.is_empty());
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
            author: node.identity.node_id()?,
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
            author: node.identity.node_id()?,
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
        let request = OperationRequest {
            operation_id: "pending-open".into(),
            network_id: network.clone(),
            conversation_id: "chat".into(),
            task_id: None,
            target_node: node.identity.node_id()?,
            session_id: Some("pending-session".into()),
            expected_revision: 0,
            basis_heads: vec![pending.hash.clone()],
            action: json!({"type":"open","executor":"claude_code","cwd":dir}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        assert_eq!(
            node.rpc(human.clone(), json!({"method":"operate","request":request}))
                .await
                .unwrap_err()
                .to_string(),
            "operation basis is incomplete or conflicting"
        );
        let mut agent_request = request.clone();
        agent_request.operation_id = "pending-agent-open".into();
        let agent = Access {
            human: false,
            ..human.clone()
        };
        assert_eq!(
            node.dispatch(agent, agent_request)
                .await
                .unwrap_err()
                .to_string(),
            "operation basis is incomplete or conflicting"
        );
        let mut command = request.clone();
        command.operation_id = "pending-command".into();
        command.session_id = None;
        command.action = json!({"type":"command","command":"printf net1 >> pending-effect","cwd":dir,"timeout_secs":1});
        assert_eq!(
            node.dispatch(human.clone(), command.clone())
                .await
                .unwrap_err()
                .to_string(),
            "operation basis is incomplete or conflicting"
        );
        assert_eq!(
            node.admit_operation(request.clone(), true)
                .await
                .unwrap_err()
                .to_string(),
            "required context is incomplete or conflicting"
        );
        assert_eq!(
            node.admit_operation(command.clone(), true)
                .await
                .unwrap_err()
                .to_string(),
            "required context is incomplete or conflicting"
        );
        let mut missing = command.clone();
        missing.operation_id = "missing-root-command".into();
        missing.basis_heads = vec![crypto::digest(b"missing basis")];
        assert_eq!(
            node.dispatch(human.clone(), missing)
                .await
                .unwrap_err()
                .to_string(),
            "operation basis is incomplete or conflicting"
        );
        let scoped_elsewhere = Access {
            conversation_id: Some("elsewhere".into()),
            ..human.clone()
        };
        assert_eq!(
            node.dispatch(scoped_elsewhere, command.clone())
                .await
                .unwrap_err()
                .to_string(),
            "outside this tool's conversation"
        );
        assert!(node.store.lock().await.operations(&network)?.is_empty());
        assert!(
            node.store
                .lock()
                .await
                .session(&network, &node.identity.node_id()?, "pending-session")?
                .is_none()
        );
        let dispatches: usize = connection.query_row("SELECT count(*) FROM events WHERE network_id=? AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.type')='dispatch_recorded'", [&network], |row| row.get(0))?;
        assert_eq!(dispatches, 0);
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
        assert!(
            !node
                .store
                .lock()
                .await
                .has_known_basis_gap(&network, std::slice::from_ref(&pending.hash))?
        );
        assert_eq!(
            node.dispatch(human.clone(), command.clone())
                .await
                .unwrap_err()
                .to_string(),
            "operation basis is incomplete or conflicting"
        );
        assert_eq!(
            node.admit_operation(command.clone(), true)
                .await
                .unwrap_err()
                .to_string(),
            "required context is incomplete or conflicting"
        );
        assert!(node.store.lock().await.operations(&network)?.is_empty());
        assert!(!dir.join("pending-effect").exists());
        node.store.lock().await.put_blob(&network, &blob, content)?;
        node.dispatch(human, command.clone()).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if node
                    .store
                    .lock()
                    .await
                    .operation(&network, &command.operation_id)?
                    .is_some_and(|record| record.state == OperationState::Succeeded)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(std::fs::read(dir.join("pending-effect"))?, b"net1");
        Ok(())
    }

    #[tokio::test]
    async fn net1_existing_unknown_operation_keeps_normalization_and_never_redispatches()
    -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = OperationRequest {
            operation_id: "unknown-original".into(),
            network_id: network.clone(),
            conversation_id: "chat".into(),
            task_id: None,
            target_node: node.identity.node_id()?,
            session_id: None,
            expected_revision: 0,
            basis_heads: node.store.lock().await.heads(&network, "chat")?,
            action: json!({"type":"command","command":"printf net1 >> unknown-effect; sleep 30","cwd":dir,"timeout_secs":1}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        node.admit_operation(request.clone(), true).await?;
        let old = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(record) = node
                    .store
                    .lock()
                    .await
                    .operation(&network, &request.operation_id)?
                    && record.state == OperationState::Unknown
                {
                    return Ok::<_, anyhow::Error>(record);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(std::fs::read(dir.join("unknown-effect"))?, b"net1");
        let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
        connection.execute(
            "UPDATE events SET status='pending' WHERE network_id=? AND hash=?",
            rusqlite::params![network, request.basis_heads[0]],
        )?;
        let mut retry = request.clone();
        retry.resources = vec![
            format!("{}/.", node.config.roots[0].display()),
            node.config.roots[0].to_string_lossy().into(),
        ];
        let repeated = node.admit_operation(retry.clone(), true).await?;
        assert_eq!(repeated["operation"], serde_json::to_value(&old)?);
        let mut changed = retry.clone();
        changed.action["command"] = json!("printf changed >> unknown-effect");
        assert_eq!(
            node.admit_operation(changed, true)
                .await
                .unwrap_err()
                .to_string(),
            "operation ID reused with different body"
        );
        let mut outside = retry;
        outside.resources = vec!["/".into()];
        assert_eq!(
            node.admit_operation(outside, true)
                .await
                .unwrap_err()
                .to_string(),
            "execution path is outside configured roots"
        );
        tokio::task::yield_now().await;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?,
            Some(old)
        );
        assert_eq!(std::fs::read(dir.join("unknown-effect"))?, b"net1");
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
            origin: node.identity.node_id()?,
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
                author: source.identity.node_id()?,
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
        let source_id = source.identity.node_id()?;
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
        let mut sources = receiver.history_sources(&network, &source_id).await?;
        let mut budget = HistoryBudget {
            events: 5,
            bytes: 8192,
        };
        assert!(
            receiver
                .history_events(&network, &mut sources, head, &mut budget)
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
        let mut sources = receiver.history_sources(&network, &source_id).await?;
        let mut budget = HistoryBudget {
            events: 128,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        let old = receiver
            .history_events(&network, &mut sources, head, &mut budget)
            .await?;
        assert_eq!(old, vec![envelopes.last().unwrap().clone()]);
        assert_eq!(batch_requests.load(Ordering::SeqCst), before);
        assert_eq!(single_requests.load(Ordering::SeqCst), 1);
        assert!(requests.load(Ordering::SeqCst) < 24);

        legacy.store(false, Ordering::SeqCst);
        malformed.store(4, Ordering::SeqCst);
        let mut sources = receiver.history_sources(&network, &source_id).await?;
        let mut budget = HistoryBudget {
            events: 128,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        let single = receiver
            .history_events(&network, &mut sources, head, &mut budget)
            .await?;
        assert_eq!(single, vec![envelopes.last().unwrap().clone()]);
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
        let backup_id = backup.identity.node_id()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !receiver.hub.online(&network).await.contains(&backup_id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        malformed.store(3, Ordering::SeqCst);
        let mut sources = receiver.history_sources(&network, &source_id).await?;
        let mut budget = HistoryBudget {
            events: 1024,
            bytes: 8 * MAX_EVENT_BYTES,
        };
        let recovered = receiver
            .history_events(&network, &mut sources, head, &mut budget)
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
        let node = Runtime::open(&dir).await?;
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
            author: node.identity.node_id()?,
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
            author: node.identity.node_id()?,
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
        let local = node.identity.node_id()?;
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
        let node = Runtime::open(&dir).await?;
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
        assert!(node.store.lock().await.operations(&network)?.is_empty());
        drop(external);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn net1_changing_heads_keep_one_scope_checkpoint_and_ignore_unverified_heads()
    -> Result<()> {
        let dir = fixture_under("network-reliability")?;
        let node = Runtime::open(&dir).await?;
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
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
            author: other.node_id()?,
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
        let node = Runtime::open(&dir).await?;
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
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
            author: author.node_id()?,
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
            author: author.node_id()?,
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
                        .contains(&nodes[3].identity.node_id().unwrap())
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
        let preferred = nodes[1].identity.node_id()?;
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
        assert!(nodes[0].store.lock().await.operations(&network)?.is_empty());
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
            author: author.node_id()?,
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
            author: author.node_id()?,
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
        let request = OperationRequest {
            operation_id: "revoked-sessionless-command".into(),
            network_id: network.clone(),
            conversation_id: "chat".into(),
            task_id: None,
            target_node: nodes[0].identity.node_id()?,
            session_id: None,
            expected_revision: 0,
            basis_heads: vec![head.hash.clone()],
            action: json!({"type":"command","command":"printf forbidden > revoked-effect","cwd":dirs[0],"timeout_secs":1}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        let dispatch = author.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "chat".into(),
            author: author.node_id()?,
            sequence: 3,
            previous: Some(head.hash.clone()),
            parents: vec![head.hash.clone()],
            created_at_ms: 3,
            body: EventBody::DispatchRecorded {
                operation_id: request.operation_id.clone(),
                task_id: None,
                target_node: request.target_node.clone(),
                session_id: None,
                action: json!(request),
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
            payload: json!({"type":"execute","human":true,"request":request,"dispatch_head":dispatch.hash}),
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
        assert_eq!(
            nodes[0]
                .store
                .lock()
                .await
                .operation(&network, &request.operation_id)?,
            None
        );
        assert!(!dirs[0].join("revoked-effect").exists());
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
                    scheduler_node: Some(nodes[0].identity.node_id()?),
                },
                vec![],
                2,
                &schedule_author.node_id()?,
                &nodes[0].identity.node_id()?,
                false,
            )?;
            (trigger, store.heads(&network, "schedule-revocation")?)
        };
        *membership_during_fetch.lock().await = Some(revoked_state(3, true)?);
        let schedule = Inbound {
            network_id: network.clone(),
            origin: nodes[2].identity.node_id()?,
            request_id: "revoked-schedule".into(),
            broadcast: false,
            payload: json!({"type":"schedule","version":1,"target_node":nodes[0].identity.node_id()?,"conversation_id":"schedule-revocation","trigger":trigger.hash,"basis_heads":schedule_basis}),
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
                &nodes[0].identity.node_id()?
            )?,
            None
        );
        let (snapshot, membership) = {
            let store = nodes[0].store.lock().await;
            store.authorize_node(
                &network,
                &schedule_author.node_id()?,
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
            &nodes[0].identity.node_id()?,
        )?;
        assert_eq!(
            admission_control
                .scheduler_job(
                    &network,
                    "schedule-revocation",
                    &trigger.hash,
                    &nodes[0].identity.node_id()?
                )?
                .unwrap()
                .state
                .name(),
            "queued"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(nodes);
        for dir in dirs {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    async fn queued(
        node: &Runtime,
        network: &str,
        operation: &str,
        action: Value,
    ) -> Result<OperationRequest> {
        let local = node.identity.node_id()?;
        let mut store = node.store.lock().await;
        store.upsert_session(
            network,
            &ExecutionSession {
                network_id: network.into(),
                target_node: local.clone(),
                session_id: operation.into(),
                revision: 0,
                state: "idle".into(),
                executor: "claude_code".into(),
                cwd: node.config.roots[0].to_string_lossy().into_owned(),
                native_session_id: Some("native-test".into()),
            },
        )?;
        let mut request = OperationRequest {
            operation_id: operation.into(),
            network_id: network.into(),
            conversation_id: "chat".into(),
            task_id: None,
            target_node: local,
            session_id: Some(operation.into()),
            expected_revision: 0,
            basis_heads: store.heads(network, "chat")?,
            action,
            resources: vec![node.config.roots[0].to_string_lossy().into_owned()],
            binding: None,
            manager_authority: None,
        };
        store.bind_test_operation(&node.identity, &mut request)?;
        store.accept_operation(network, &request)?;
        Ok(request)
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
                "scheduler_node":node.identity.node_id()?}),
            )
            .await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn unsupported_epoch_does_not_open_databases_or_modify_the_directory() -> Result<()> {
        let dir = fixture()?;
        let config = std::fs::read(dir.join("config.json"))?;
        for epoch in [None, Some(1), Some(2), Some(4)] {
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
    async fn membership_reopen_recovers_either_durable_store_without_root_secrets() -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        assert!(!node.has_membership().await?);
        let owner = test_owner(&node)?;
        let initial = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        node.identities.init_network(initial.clone())?;
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
        node.store
            .lock()
            .await
            .install_trust_bundle(&owner.fixture_network_id()?, &updated)?;
        drop(node);
        let node = Runtime::open(&dir).await?;
        assert_eq!(node.membership_state().await?, updated);
        assert_eq!(node.identities.public_state()?, updated);
        assert!(!dir.join("auth.sqlite3").exists());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn executor_followup_only_accepts_its_authorized_dispatched_result() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        task(&node, &network, "task", true).await?;
        task(&node, &network, "unapproved", false).await?;
        let executor = LocalIdentity::generate();
        let origin = executor.node_id()?;
        let owner = test_owner(&node)?;
        let mut store = node.store.lock().await;
        let mut nodes = store
            .trust_bundle(&network)?
            .unwrap()
            .project()?
            .members
            .into_iter()
            .filter(|member| member.kind == IdentityKind::Node)
            .map(|member| member.certificate())
            .collect::<Result<Vec<_>>>()?;
        nodes.push(executor.certificate(vec![NodeRole::Replica, NodeRole::Executor])?);
        store.install_trust_bundle(&network, &owner.sign_network_fixture(2, nodes)?)?;
        let mut request = OperationRequest {
            operation_id: "operation".into(),
            network_id: network.clone(),
            conversation_id: "chat".into(),
            task_id: Some("task".into()),
            target_node: origin.clone(),
            session_id: Some("session".into()),
            expected_revision: 0,
            basis_heads: store.heads(&network, "chat")?,
            action: json!({"type":"send","text":"test"}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        store.bind_test_operation(&node.identity, &mut request)?;
        let scheduler = node.identity.node_id()?;
        store.append_event(
            &node.identity,
            &network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: request.operation_id.clone(),
                task_id: request.task_id.clone(),
                target_node: origin.clone(),
                session_id: request.session_id.clone(),
                action: json!(request),
            },
            vec![],
            now_ms(),
        )?;
        let result = EventBody::ExecutionObserved {
            operation_id: request.operation_id.clone(),
            task_id: request.task_id.clone(),
            target_node: origin.clone(),
            session_id: request.session_id.clone(),
            status: "succeeded".into(),
            detail: json!({"result":"done"}),
        };
        let envelope = store.append_event(
            &executor,
            &network,
            "chat",
            result.clone(),
            vec![],
            now_ms(),
        )?;
        store.validate_followup_result(&network, "chat", &envelope.hash, &scheduler)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &envelope.hash, &origin)
                .is_err()
        );
        assert!(
            store
                .validate_followup_result(&network, "another-chat", &envelope.hash, &scheduler)
                .is_err()
        );
        let message = store.append_event(
            &executor,
            &network,
            "chat",
            EventBody::MessagePosted {
                message_id: "forged-user".into(),
                role: "user".into(),
                text: "wake manager".into(),
                scheduler_node: Some(node.identity.node_id()?),
            },
            vec![],
            now_ms(),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &message.hash, &scheduler)
                .is_err()
        );
        for (operation, target, status) in [
            ("not-dispatched", origin.as_str(), "succeeded"),
            ("operation", "another-executor", "succeeded"),
            ("operation", origin.as_str(), "unknown"),
        ] {
            let mut invalid = result.clone();
            if let EventBody::ExecutionObserved {
                operation_id,
                target_node,
                status: state,
                ..
            } = &mut invalid
            {
                *operation_id = operation.into();
                *target_node = target.into();
                *state = status.into();
            }
            let invalid =
                store.append_event(&executor, &network, "chat", invalid, vec![], now_ms())?;
            assert!(
                store
                    .validate_followup_result(&network, "chat", &invalid.hash, &scheduler)
                    .is_err()
            );
        }
        let mut unapproved = request.clone();
        unapproved.operation_id = "unapproved-operation".into();
        unapproved.task_id = Some("unapproved".into());
        unapproved.binding = None;
        store.bind_test_operation(&node.identity, &mut unapproved)?;
        store.append_event(
            &node.identity,
            &network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: unapproved.operation_id.clone(),
                task_id: unapproved.task_id.clone(),
                target_node: origin.clone(),
                session_id: unapproved.session_id.clone(),
                action: json!(unapproved),
            },
            vec![],
            now_ms(),
        )?;
        let invalid = store.append_event(
            &executor,
            &network,
            "chat",
            EventBody::ExecutionObserved {
                operation_id: unapproved.operation_id,
                task_id: unapproved.task_id,
                target_node: origin.clone(),
                session_id: unapproved.session_id,
                status: "succeeded".into(),
                detail: json!({}),
            },
            vec![],
            now_ms(),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &invalid.hash, &scheduler)
                .is_err()
        );
        store.append_event(
            &node.identity,
            &network,
            "chat",
            EventBody::TaskChanged {
                task_id: "task".into(),
                status: "cancelled".into(),
                detail: "user stopped".into(),
            },
            vec![],
            now_ms(),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &envelope.hash, &scheduler)
                .is_err()
        );
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn unstarted_followup_survives_reopen_and_claimed_attempt_is_not_repeated() -> Result<()>
    {
        use crate::scheduling::{JobState, TriggerKind};

        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
        task(&node, &network, "task", false).await?;
        let mut request = queued(
            &node,
            &network,
            "seed",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, "seed")?;
            store.finish_operation(&network, "seed", OperationState::Failed, json!({}))?;
            request.operation_id = "followup".into();
            request.task_id = Some("task".into());
            request.expected_revision = store
                .session(
                    &network,
                    &request.target_node,
                    request
                        .session_id
                        .as_deref()
                        .context("execution session is required")?,
                )?
                .unwrap()
                .revision;
            request.basis_heads = store.heads(&network, "chat")?;
            store.bind_test_operation(&node.identity, &mut request)?;
            store.append_event(
                &node.identity,
                &network,
                "chat",
                EventBody::DispatchRecorded {
                    operation_id: request.operation_id.clone(),
                    task_id: request.task_id.clone(),
                    target_node: request.target_node.clone(),
                    session_id: request.session_id.clone(),
                    action: json!(request),
                },
                vec![],
                now_ms(),
            )?;
            store.accept_operation(&network, &request)?;
            store.claim_operation(&network, &request.operation_id)?;
        }
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )
        .await?;
        node.flush_completions(&network).await?;
        let marker_key = "followup-wait:v1:followup";
        let result_hash = {
            let mut store = node.store.lock().await;
            let result = store
                .context(&network, "chat", &[])?
                .events
                .into_iter()
                .find(|event| {
                    matches!(&event.body, EventBody::ExecutionObserved { operation_id, status, .. }
                        if operation_id == "followup" && status == "succeeded")
                })
                .context("completed operation has no durable result")?;
            let result_hash = crypto::digest(&serde_json::to_vec(&result)?);
            assert!(
                store
                    .local_value(&network, "completion:followup")?
                    .is_none()
            );
            assert_eq!(
                store.local_value(&network, marker_key)?.unwrap()["trigger"],
                result_hash
            );
            assert!(store.scheduler_jobs(&network, "chat", &local)?.is_empty());
            store.seal_binding(&node.identity, request.binding.as_ref().unwrap())?;
            result_hash
        };
        drop(node);

        let node = Arc::new(Runtime::open(&dir).await?);
        node.flush_completions(&network).await?;
        {
            let store = node.store.lock().await;
            assert!(
                store
                    .local_value(&network, "completion:followup")?
                    .is_none()
            );
            assert_eq!(
                store.local_value(&network, marker_key)?.unwrap()["trigger"],
                result_hash
            );
            assert!(store.scheduler_jobs(&network, "chat", &local)?.is_empty());
        }
        let grant = node
            .set_followup(
                &Access {
                    network_id: network.clone(),
                    human: true,
                    conversation_id: Some("chat".into()),
                    manager: None,
                },
                "chat",
                &json!({"task_id":"task","mode":"report","expected_version":0,
                    "scheduler_node":local}),
            )
            .await?;
        assert_eq!(grant["policy"]["mode"], "report");
        assert_eq!(grant["policy"]["version"], 1);
        assert!(node.maybe_followup(&request, &result_hash).await?);
        assert!(node.maybe_followup(&request, &result_hash).await?);
        let queued_job = {
            let store = node.store.lock().await;
            assert!(store.local_value(&network, marker_key)?.is_none());
            let jobs = store.scheduler_jobs(&network, "chat", &local)?;
            assert_eq!(jobs.len(), 1);
            let job = jobs.into_iter().next().unwrap();
            assert_eq!(job.trigger, result_hash);
            assert_eq!(job.kind, TriggerKind::Autonomous);
            assert_eq!(job.state, JobState::Queued);
            job
        };
        drop(node);

        let node = Arc::new(Runtime::open(&dir).await?);
        let attempt = {
            let mut store = node.store.lock().await;
            assert_eq!(
                store.scheduler_jobs(&network, "chat", &local)?,
                vec![queued_job.clone()]
            );
            let claimed = store
                .claim_next(&network, "chat", &local, "followup-boot-a", "test-sdk")?
                .context("authorized result was not queued")?;
            assert_eq!(claimed.job.trigger, result_hash);
            assert_eq!(claimed.job.sequence, queued_job.sequence);
            assert_eq!(claimed.job.message_id, queued_job.message_id);
            let attempt = claimed.job.state.attempt().unwrap().clone();
            assert!(attempt.read_only);
            assert_eq!(attempt.followup_version, Some(1));
            let started =
                store.mark_started(&network, "chat", &local, &result_hash, &attempt.id)?;
            assert_eq!(
                started.state,
                JobState::Running {
                    attempt: attempt.clone()
                }
            );
            attempt
        };
        drop(node);

        let node = Arc::new(Runtime::open(&dir).await?);
        let recovered = {
            let mut store = node.store.lock().await;
            assert!(
                store
                    .claim_next(&network, "chat", &local, "followup-boot-b", "test-sdk")?
                    .is_none()
            );
            let job = store
                .scheduler_job(&network, "chat", &result_hash, &local)?
                .context("started follow-up was lost after reopen")?;
            assert_eq!(
                job.state,
                JobState::Unknown {
                    attempt,
                    detail:
                        "Node restarted after the SDK start boundary; this call was not replayed"
                            .into(),
                    fenced: false,
                }
            );
            job
        };
        node.flush_completions(&network).await?;
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            assert_eq!(
                store.scheduler_jobs(&network, "chat", &local)?,
                vec![recovered]
            );
            assert!(
                store
                    .local_value(&network, "completion:followup")?
                    .is_none()
            );
            assert!(store.local_value(&network, marker_key)?.is_none());
            assert!(
                store
                    .claim_next(&network, "chat", &local, "followup-boot-b", "test-sdk")?
                    .is_none()
            );
            assert_eq!(
                store
                    .context(&network, "chat", &[])?
                    .events
                    .iter()
                    .filter(|event| {
                        matches!(&event.body, EventBody::ExecutionObserved { operation_id, status, .. }
                            if operation_id == "followup" && status == "succeeded")
                    })
                    .count(),
                1
            );
        }
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
            let request = OperationRequest {
                operation_id: format!("new-{status}"),
                network_id: network.clone(),
                conversation_id: "chat".into(),
                task_id: Some("task".into()),
                target_node: node.identity.node_id()?,
                session_id: Some(format!("session-{status}")),
                expected_revision: 0,
                basis_heads: node.store.lock().await.heads(&network, "chat")?,
                action: json!({"type":"open","executor":"claude_code","cwd":dir}),
                resources: vec![],
                binding: None,
                manager_authority: None,
            };
            assert!(
                node.dispatch(agent.clone(), request.clone())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("only the user may reopen")
            );
            assert!(
                node.admit_operation(request, false)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("only the user may reopen")
            );
            node.rpc(human.clone(), update).await?;
        }
        assert!(node.store.lock().await.operations(&network)?.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn agent_cannot_silently_detach_a_session_from_its_latest_admitted_task() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        task(&node, &network, "task", false).await?;
        task(&node, &network, "next-task", false).await?;
        let human = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: Some("chat".into()),

            manager: None,
        };
        let mut associated = queued(
            &node,
            &network,
            "seed",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, "seed")?;
            store.finish_operation(&network, "seed", OperationState::Succeeded, json!({}))?;
            associated.operation_id = "associated".into();
            associated.task_id = Some("task".into());
            associated.expected_revision = 1;
            associated.binding = None;
            store.bind_test_operation(&node.identity, &mut associated)?;
            store.accept_operation(&network, &associated)?;
            let mut concurrent = associated.clone();
            concurrent.operation_id = "concurrent-untasked-steer".into();
            concurrent.task_id = Some("task".into());
            concurrent.action = json!({"type":"steer","turn_id":"turn","text":"test"});
            concurrent.resources.clear();
            store.accept_control_operation(&network, &concurrent)?;
            for operation in [&associated, &concurrent] {
                store.claim_operation(&network, &operation.operation_id)?;
            }
            for operation in [&associated, &concurrent] {
                store.finish_operation(
                    &network,
                    &operation.operation_id,
                    OperationState::Succeeded,
                    json!({}),
                )?;
            }
        }
        let mut request = associated.clone();
        request.operation_id = "untasked".into();
        request.task_id = None;
        request.expected_revision = 2;
        request.resources.clear();
        for status in ["working", "accepted", "cancelled"] {
            node.rpc(human.clone(), json!({"method":"update_task","conversation_id":"chat","task_id":"task","status":status})).await?;
            for action in [
                json!({"type":"steer","turn_id":"turn","text":"test"}),
                json!({"type":"send","text":"test"}),
                json!({"type":"open","executor":"claude_code","cwd":dir}),
            ] {
                request.action = action;
                assert!(
                    node.admit_operation(request.clone(), false)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("task_id is required")
                );
            }
        }
        assert_eq!(
            node.admit_operation(associated.clone(), false).await?["operation"]["state"],
            "succeeded"
        );
        let mut unadmitted = request.clone();
        unadmitted.operation_id = "unadmitted".into();
        unadmitted.expected_revision = 99;
        node.append(
            &network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: unadmitted.operation_id.clone(),
                task_id: None,
                target_node: unadmitted.target_node.clone(),
                session_id: unadmitted.session_id.clone(),
                action: json!(unadmitted),
            },
        )
        .await?;
        assert!(node.admit_operation(request.clone(), false).await.is_err());
        request.action = json!({"type":"steer","turn_id":"turn","text":"test"});
        for (operation, human, task_id) in [
            ("retask", false, Some("next-task")),
            ("human-detach", true, None),
            ("agent-after-detach", false, None),
        ] {
            request.operation_id = operation.into();
            request.task_id = task_id.map(String::from);
            request.basis_heads = node.store.lock().await.heads(&network, "chat")?;
            assert!(node.admit_operation(request.clone(), human).await.is_err());
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
        let request = queued(
            &node,
            &network,
            "unknown-role-test",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, &request.operation_id)?;
            store.finish_operation(
                &network,
                &request.operation_id,
                OperationState::Unknown,
                json!({}),
            )?;
        }
        for (origin, human, expected) in [
            (&scheduler, true, "origin cannot assert human authority"),
            (
                &scheduler,
                false,
                "history is Unreachable: 1 events, 0 blobs, 0 conflicts",
            ),
            (
                &owner,
                true,
                "history is Unreachable: 1 events, 0 blobs, 0 conflicts",
            ),
        ] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.node_id()?,
                request_id: "approval-origin-test".into(),
                payload: json!({"type":"execute","human":human,"request":request,"dispatch_head":"f".repeat(64)}),
                broadcast: false,
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                expected
            );
        }
        let missing = crypto::digest(b"not present on the receiver");
        let schedule = json!({"type":"schedule","version":1,"target_node":node.identity.node_id()?,
            "conversation_id":"chat","trigger":missing,"basis_heads":[missing]});
        let resolution = json!({"type":"resolve_unknown","target_node":node.identity.node_id()?,"operation_id":request.operation_id,"outcome":"succeeded","evidence":"verified test fixture process stopped"});
        for origin in [&replica, &relay] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.node_id()?,
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
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        for origin in [&owner, &scheduler] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.node_id()?,
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
            origin: owner.node_id()?,
            request_id: "gateway-test".into(),
            payload: resolution,
            broadcast: false,
        };
        assert_eq!(
            node.handle_peer(&message).await?["operation"]["state"],
            "succeeded"
        );
        let message = Inbound {
            origin: replica.node_id()?,
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
            assert_eq!(receipt["receipt"]["target_node"], node.identity.node_id()?);
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
                .scheduler_jobs(&network, "chat", &node.identity.node_id()?)?
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
                .scheduler_jobs(&network, "chat", &node.identity.node_id()?)?
                .len(),
            1
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn shared_execution_session_is_projected_once_for_multiple_bound_tasks() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
        for task_id in ["a-old", "b-new"] {
            task(&node, &network, task_id, false).await?;
            let mut store = node.store.lock().await;
            let creation = store.task_creation(&network, "chat", task_id)?;
            store.bind_task(
                &node.identity,
                &crate::binding::TaskBinding {
                    network_id: network.clone(),
                    conversation_id: "chat".into(),
                    task_ref: creation.hash,
                    task_id: task_id.into(),
                    authority_node: local.clone(),
                    generation: 1,
                    target_node: local.clone(),
                    session_id: "shared".into(),
                    open_spec: json!({"type":"open","executor":"codex","cwd":dir}),
                    previous_binding_hash: None,
                    previous_seal: None,
                },
            )?;
        }
        node.store.lock().await.upsert_session(
            &network,
            &ExecutionSession {
                network_id: network.clone(),
                target_node: local.clone(),
                session_id: "shared".into(),
                revision: 0,
                state: "idle".into(),
                executor: "codex".into(),
                cwd: dir.to_string_lossy().into_owned(),
                native_session_id: None,
            },
        )?;
        let snapshot = node
            .rpc(
                Access {
                    network_id: network,
                    human: true,
                    conversation_id: None,
                    manager: None,
                },
                json!({"method":"conversation","conversation_id":"chat"}),
            )
            .await?;
        assert_eq!(
            snapshot["tasks"].as_array().context("tasks missing")?.len(),
            2
        );
        assert_eq!(
            snapshot["sessions"]
                .as_array()
                .context("sessions missing")?
                .len(),
            1
        );
        assert!(
            snapshot["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|task| task["binding"]["session_id"] == "shared")
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
            "mode":"report","expected_version":0,"scheduler_node":node.identity.node_id()?});
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
    async fn completion_receipt_recovers_history_and_notifies_once_after_reopen() -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "done",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        let result = json!({"result":"verified completion"});
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, "done")?;
            store.local_set(
                &network,
                "completion:done",
                &json!({"state":"succeeded","result":result}),
            )?;
            store.finish_operation(&network, "done", OperationState::Succeeded, result)?;
        }
        drop(node);
        let node = Arc::new(Runtime::open(&dir).await?);
        node.flush_completions(&network).await?;
        node.flush_completions(&network).await?;
        let store = node.store.lock().await;
        let events = store.context(&network, "chat", &[])?.events;
        assert_eq!(events.iter().filter(|e|matches!(&e.body,EventBody::ExecutionObserved{operation_id,status,..} if operation_id==&request.operation_id && status=="succeeded")).count(),1);
        assert_eq!(store.pending_notifications(&network, "client")?.len(), 1);
        assert!(store.local_value(&network, "completion:done")?.is_none());
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn failed_claude_spawn_does_not_leave_a_nonexistent_native_session() -> Result<()> {
        let dir = fixture()?;
        let mut cfg = Config::load(&dir)?;
        cfg.claude_bin = dir.join("missing-claude");
        cfg.save(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "not-started",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            let mut session = store
                .session(
                    &network,
                    &request.target_node,
                    request
                        .session_id
                        .as_deref()
                        .context("execution session is required")?,
                )?
                .unwrap();
            session.native_session_id = None;
            store.upsert_session(&network, &session)?;
        }
        node.clone().execute_operation(request.clone()).await?;
        let store = node.store.lock().await;
        assert_eq!(
            store
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Failed
        );
        assert!(
            store
                .session(
                    &network,
                    &request.target_node,
                    request
                        .session_id
                        .as_deref()
                        .context("execution session is required")?
                )?
                .unwrap()
                .native_session_id
                .is_none()
        );
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn reconciliation_reports_only_the_executors_confirmed_absence() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
        let result = node.reconcile(&network, &local, "never-received").await?;
        assert_eq!(result["state"], "not_found");
        assert_eq!(result["target_node"], local);
        assert!(result["operation"].is_null());
        assert!(result["checked_at_ms"].as_i64().is_some_and(|t| t > 0));
        assert!(
            node.reconcile(&network, "offline-node", "never-received")
                .await
                .is_err()
        );
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
        request["scheduler_node"] = json!(target.node_id()?);
        let first = node.rpc(access.clone(), request.clone()).await?;
        assert_eq!(first["state"], "pending_delivery");
        assert_eq!(first["accepted"], true);
        assert_eq!(first["queued"], false);
        let retry = node.rpc(access.clone(), request.clone()).await?;
        assert_eq!(retry["receipt"], first["receipt"]);
        assert_eq!(retry["trigger"], first["trigger"]);
        request["scheduler_node"] = json!(other.node_id()?);
        let changed = node.rpc(access.clone(), request).await.unwrap_err();
        assert!(changed.to_string().contains("different content or target"));
        let store = node.store.lock().await;
        assert_eq!(store.messages(&network, "chat")?.len(), 1);
        let pending = store.scheduler_outbox(&network)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_node, target.node_id()?);
        assert_eq!(pending[0].trigger, first["trigger"].as_str().unwrap());
        assert!(!pending[0].delivered);
        drop(store);
        let fresh = node.rpc(access, json!({"method":"message","conversation_id":"chat","message_id":"fresh-message","text":"new request"})).await?;
        assert_eq!(fresh["scheduler_node"], node.identity.node_id()?);
        assert_ne!(fresh["scheduler_node"], first["scheduler_node"]);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn completion_requires_the_exact_persisted_turn_even_after_unknown() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "uncertain",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            let mut session = store
                .session(
                    &network,
                    &request.target_node,
                    request
                        .session_id
                        .as_deref()
                        .context("execution session is required")?,
                )?
                .unwrap();
            session.executor = "codex".into();
            store.upsert_session(&network, &session)?;
            store.claim_operation(&network, &request.operation_id)?;
        }
        node.finish(
            &request,
            OperationState::Unknown,
            json!({"error":"start response lost"}),
        )
        .await?;
        let completion = json!({"method":"turn/completed","params":{"threadId":"native-test","turn":{"id":"unrelated","status":"completed"}}});
        node.observe_engine(completion.clone()).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        node.store.lock().await.local_set(
            &network,
            "turn:uncertain",
            &json!({"turn":{"id":"expected"}}),
        )?;
        node.observe_engine(completion.clone()).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        let mut matching = completion;
        matching["params"]["turn"]["id"] = json!("expected");
        node.observe_engine(matching).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Succeeded
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn accepted_recovery_rechecks_config_and_rejects_before_any_external_call() -> Result<()>
    {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "queued",
            json!({"type":"send","text":"must not run"}),
        )
        .await?;
        drop(node);
        let mut cfg = Config::load(&dir)?;
        cfg.roles.retain(|r| r != &NodeRole::Executor);
        cfg.save(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        assert!(node.clone().execute_operation(request).await.is_err());
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, "queued")?
                .unwrap()
                .state,
            OperationState::Rejected
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn stale_accepted_send_is_rejected_after_a_control_advances_revision() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "send",
            json!({"type":"send","text":"must not run"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            let mut control = request.clone();
            control.operation_id = "control".into();
            control.resources.clear();
            control.action = json!({"type":"interrupt","turn_id":"turn"});
            store.accept_control_operation(&network, &control)?;
            store.claim_operation(&network, "control")?;
            store.finish_operation(&network, "control", OperationState::Succeeded, json!({}))?;
        }
        assert!(node.clone().execute_operation(request).await.is_err());
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, "send")?
                .unwrap()
                .state,
            OperationState::Rejected
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn maintenance_waits_for_local_outcomes_and_completion_but_not_remote_operations()
    -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        let owner = LocalIdentity::generate();
        let remote = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        node.install_membership(owner.sign_network_fixture(
            1,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                remote.certificate(vec![NodeRole::Executor, NodeRole::Replica])?,
            ],
        )?)
        .await?;
        node.append(
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "test".into(),
            },
        )
        .await?;
        node.maintenance_idle().await?;
        let request = queued(
            &node,
            &network,
            "local",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        assert!(
            node.maintenance_idle()
                .await
                .unwrap_err()
                .downcast_ref::<Busy>()
                .is_some()
        );
        node.store.lock().await.claim_operation(&network, "local")?;
        assert!(node.maintenance_idle().await.is_err());
        node.store.lock().await.finish_operation(
            &network,
            "local",
            OperationState::Unknown,
            json!({}),
        )?;
        assert!(node.maintenance_idle().await.is_err());
        node.store.lock().await.finish_operation(
            &network,
            "local",
            OperationState::Succeeded,
            json!({}),
        )?;
        node.maintenance_idle().await?;
        node.store
            .lock()
            .await
            .local_set(&network, "completion:local", &json!({}))?;
        assert!(node.maintenance_idle().await.is_err());
        {
            let mut store = node.store.lock().await;
            store.local_delete(&network, "completion:local")?;
            let mut remote_request = request;
            remote_request.operation_id = "remote".into();
            remote_request.target_node = remote.node_id()?;
            remote_request.resources.clear();
            remote_request.task_id = Some("remote-fixture".into());
            remote_request.binding = None;
            store.bind_test_operation(&node.identity, &mut remote_request)?;
            store.accept_operation(&network, &remote_request)?;
        }
        node.maintenance_idle().await?;
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
    node_id: String,
    batch: Option<bool>,
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

pub struct Runtime {
    pub config: Config,
    pub identity: LocalIdentity,
    pub store: Arc<Mutex<Store>>,
    pub identities: Arc<IdentityStore>,
    pub network: Arc<NetworkHub>,
    pub hub: Arc<PeerHub>,
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
    completion_lock: Mutex<()>,
    progress_sequences: std::sync::Mutex<HashMap<(String, String), u64>>,
    pub engine: Engine,
    _codex_process: Option<tokio::process::Child>,
    _process_lock: Arc<std::fs::File>,
    peer_rx: Mutex<Option<mpsc::Receiver<Inbound>>>,
    network_rx: Mutex<Option<mpsc::Receiver<IncomingSocket>>>,
    engine_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
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
            epoch == json!({"data_epoch":3}),
            "this Node requires an initialized data epoch 3 directory"
        );
        let mut config = Config::load(dir)?;
        let process_lock = Arc::new(config::lock_data_dir(dir)?);
        let blob_staging = blob_transfer::BlobStaging::open(dir, process_lock.clone())?;
        let identity = config::identity(dir)?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.attach_process_lock(process_lock.clone());
        let identities = Arc::new(IdentityStore::open(&dir.join("identity.sqlite3"))?);
        let networks = store.networks()?;
        ensure!(
            networks.len() <= 1,
            "one installation can join only one personal network"
        );
        let stored = networks
            .first()
            .map(|network| store.trust_bundle(network))
            .transpose()?
            .flatten();
        let local = if identities.has_network()? {
            Some(identities.public_state()?)
        } else {
            None
        };
        let state = match (local, stored) {
            (Some(a), Some(b)) => Some(if a.epoch() >= b.epoch() {
                b.merge(&a)?
            } else {
                a.merge(&b)?
            }),
            (a, b) => a.or(b),
        };
        if let Some(state) = state {
            let view = state.project()?;
            store.install_trust_bundle(&view.network_id, &state)?;
            identities.install_state(state)?;
            if view.authorize_node(&identity.node_id()?, None).is_ok() {
                store.save_local_identity(&view.network_id, &identity)?;
            }
        }
        let store = Arc::new(Mutex::new(store));
        let (hub, peer_rx) = PeerHub::new(identity.clone(), store.clone(), &config.roles);
        let (network, network_rx) = NetworkHub::new(identity.clone(), config.network.clone());
        let (events, _) = broadcast::channel(256);
        let (tx, rx) = mpsc::unbounded_channel();
        let managed_socket = config.codex_socket.is_none();
        if managed_socket {
            let socket_dir =
                PathBuf::from("/tmp").join(format!("vibeface-{}", &identity.node_id()?[..32]));
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
        if managed_socket && config.roles.contains(&NodeRole::Executor) {
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
                        tracing::warn!(error=%error,"Codex not available; configure an executor before sending tasks")
                    }
                }
            }
        }
        let engine = Engine::new(
            ExecutorConfig {
                codex_socket: config.codex_socket.clone(),
                claude_bin: config.claude_bin.clone(),
                cursor_bin: config.cursor_bin.clone(),
                timeout_secs: config.timeout_secs,
            },
            tx,
        );
        let node = Self {
            config,
            identity,
            store,
            identities,
            network,
            hub,
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            manager_controls: Mutex::new(HashMap::new()),
            agent_blobs: Mutex::new(HashMap::new()),
            blob_staging,
            schedule_workers: Mutex::new(BTreeSet::new()),
            schedule_wakeup: tokio::sync::Notify::new(),
            observation: std::sync::Mutex::new(Default::default()),
            tool_url: std::sync::OnceLock::new(),
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            completion_lock: Mutex::new(()),
            progress_sequences: std::sync::Mutex::new(HashMap::new()),
            engine,
            _codex_process: codex_process,
            _process_lock: process_lock,
            peer_rx: Mutex::new(Some(peer_rx)),
            network_rx: Mutex::new(Some(network_rx)),
            engine_rx: Mutex::new(Some(rx)),
        };
        node.recover_blob_staging(MAX_BLOB_BYTES as u64).await;
        Ok(node)
    }
    pub async fn install_membership(&self, incoming: NetworkState) -> Result<AuthorizationView> {
        let _work = self.maintenance.enter()?;
        let mut store = self.store.lock().await;
        let mut state = incoming;
        let networks = store.networks()?;
        ensure!(
            networks
                .iter()
                .all(|network| network == &state.genesis.network_id),
            "installation belongs to a different personal network"
        );
        if self.identities.has_network()? {
            let previous = self.identities.public_state()?;
            state = previous.merge(&state)?;
        }
        if let Some(previous) = store.trust_bundle(&state.genesis.network_id)? {
            state = previous.merge(&state)?;
        }
        let view = state.project()?;
        self.identities.install_state(state.clone())?;
        store.install_trust_bundle(&view.network_id, &state)?;
        if view.authorize_node(&self.identity.node_id()?, None).is_ok() {
            store.save_local_identity(&view.network_id, &self.identity)?;
        }
        Ok(view)
    }

    pub async fn has_membership(&self) -> Result<bool> {
        let in_identity = self.identities.has_network()?;
        let in_store = !self.store.lock().await.networks()?.is_empty();
        if !in_identity && !in_store {
            return Ok(false);
        }
        self.membership_state().await?;
        Ok(true)
    }

    pub async fn membership_state(&self) -> Result<NetworkState> {
        let local = if self.identities.has_network()? {
            Some(self.identities.public_state()?)
        } else {
            None
        };
        let mut store = self.store.lock().await;
        let networks = store.networks()?;
        ensure!(
            networks.len() <= 1,
            "one installation can join only one personal network"
        );
        let stored = networks
            .first()
            .map(|network| store.trust_bundle(network))
            .transpose()?
            .flatten();
        let merged = match (&local, &stored) {
            (Some(a), Some(b)) => {
                if a.epoch() >= b.epoch() {
                    b.merge(a)?
                } else {
                    a.merge(b)?
                }
            }
            (Some(state), None) | (None, Some(state)) => state.clone(),
            (None, None) => bail!("personal network is not installed"),
        };
        if stored.as_ref() != Some(&merged) || local.as_ref() != Some(&merged) {
            let _work = self.maintenance.enter()?;
            self.identities.install_state(merged.clone())?;
            store.install_trust_bundle(&merged.genesis.network_id, &merged)?;
            let view = merged.project()?;
            if view.authorize_node(&self.identity.node_id()?, None).is_ok() {
                store.save_local_identity(&view.network_id, &self.identity)?;
            }
        }
        Ok(merged)
    }
    pub fn info(&self) -> Value {
        json!({"node_id":self.identity.node_id().ok(),"version":env!("CARGO_PKG_VERSION"),"roles":self.config.roles,"manager_available":self.config.manager.is_some()})
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        self.start_schedule_delivery();
        self.start_observation_worker();
        let restore = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                if let Err(error) = restore.restore_execution_subscriptions().await {
                    tracing::debug!(%error,"execution subscription recovery pending");
                }
            }
        });
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
                    if !message.broadcast {
                        let value = response.unwrap_or_else(|e| json!({"error":format!("{e:#}")}));
                        let _ = n.hub.respond(&message, value).await;
                    }
                });
            }
        });
        let mut receiver = self
            .engine_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if let Err(e) = node.observe_engine(event).await {
                    tracing::warn!(error=%e,"could not persist executor event");
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
            let connecting = Arc::new(Mutex::new(BTreeSet::new()));
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let Ok(state) = node.identities.public_state() else {
                    continue;
                };
                let Ok(view) = state.project() else { continue };
                let online = node.hub.online(&view.network_id).await;
                for member in view.members.into_iter().filter(|member| {
                    member.kind == IdentityKind::Node && !view.revoked.contains(&member.subject_id)
                }) {
                    if member.subject_id == node.identity.node_id().unwrap_or_default()
                        || online.contains(&member.subject_id)
                    {
                        continue;
                    }
                    let key = (view.network_id.clone(), member.subject_id.clone());
                    if !connecting.lock().await.insert(key.clone()) {
                        continue;
                    }
                    let node = node.clone();
                    let connecting = connecting.clone();
                    tokio::spawn(async move {
                        if let Err(error) = node
                            .hub
                            .clone()
                            .connect_target(key.0.clone(), key.1.clone(), node.network.clone())
                            .await
                        {
                            tracing::debug!(%error,"personal peer reconnect pending");
                        }
                        connecting.lock().await.remove(&key);
                    });
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
                    let _ = node.sync_all(&network).await;
                    let _ = node.flush_completions(&network).await;
                }
            }
        });
        if self.maintenance.is_open() {
            self.recover_operations().await?;
        } else {
            let node = self.clone();
            tokio::spawn(async move {
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.recover_operations().await {
                    tracing::warn!(%error, "deferred operation recovery failed");
                }
            });
        }
        Ok(())
    }
    async fn recover_operations(self: &Arc<Self>) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            self.flush_completions(&network).await?;
            let operations = self.store.lock().await.operations(&network)?;
            for operation in operations {
                if operation.state == OperationState::Unknown {
                    self.finish(&operation.request,OperationState::Unknown,if operation.result.is_null(){json!({"error":"Node restarted while the external outcome was unresolved; reconcile before retrying"})}else{operation.result}).await?;
                    continue;
                }
                if operation.state != OperationState::Accepted {
                    continue;
                }
                let node = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = node.execute_operation(operation.request).await {
                        tracing::warn!(error=%e,"accepted operation recovery failed");
                    }
                });
            }
        }
        Ok(())
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        let local = self.identity.node_id()?;
        let store = self.store.lock().await;
        for network in store.networks()? {
            for operation in store.operations(&network)? {
                if operation.request.target_node != local {
                    continue;
                }
                ensure!(
                    operation.state.terminal(),
                    Busy(format!(
                        "local operation {} is {:?}",
                        operation.request.operation_id, operation.state
                    ))
                );
                ensure!(
                    store
                        .local_value(
                            &network,
                            &format!("completion:{}", operation.request.operation_id)
                        )?
                        .is_none(),
                    Busy("execution completion is pending".into())
                );
            }
            crate::scheduler::maintenance_idle(&store, &network)?;
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
        store.authorize_node(&access.network_id, &self.identity.node_id()?, None)?;
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
            .authorize_node(network, &self.identity.node_id()?, Some(role))?;
        Ok(())
    }
    async fn peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let reply = self.hub.request(network, target, payload).await?;
        if let Some(error) = reply["error"].as_str() {
            bail!("{error}");
        }
        Ok(reply)
    }
    pub async fn sync_all(&self, network: &str) -> Result<SyncReport> {
        let _work = self.maintenance.enter()?;
        self.require_role(network, NodeRole::Replica).await?;
        let local = self.identity.node_id()?;
        let mut peers = self
            .hub
            .online(network)
            .await
            .into_iter()
            .filter(|peer| peer != &local)
            .collect::<Vec<_>>();
        {
            let store = self.store.lock().await;
            peers.retain(|peer| {
                store
                    .authorize_node(network, peer, Some(NodeRole::Replica))
                    .is_ok()
            });
        }
        let source_count = peers.len();
        let reports = futures_util::stream::iter(peers)
            .map(|target| async move {
                let result = self.sync_from(network, &target).await;
                (target, result)
            })
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
        let mut scopes = Vec::new();
        let mut unreachable_sources = Vec::new();
        for (target, result) in reports {
            match result {
                Ok(report) => scopes.extend(report),
                Err(error) => {
                    tracing::debug!(network, target, error=%error, "history inventory unreachable");
                    unreachable_sources.push(target);
                }
            }
        }
        let state = if source_count == 0 || unreachable_sources.len() == source_count {
            SyncState::Unreachable
        } else if !unreachable_sources.is_empty()
            || scopes
                .iter()
                .any(|scope| scope.state != SyncState::Complete)
        {
            SyncState::Partial
        } else {
            SyncState::Complete
        };
        Ok(SyncReport {
            state,
            scopes,
            unreachable_sources,
        })
    }
    async fn read_peer(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.peer_request(network, target, payload),
        )
        .await
        .context("history peer is currently unreachable")?
    }
    async fn history_response(
        &self,
        network: &str,
        target: &str,
        payload: Value,
        budget: &mut HistoryBudget,
    ) -> Result<Value> {
        self.store
            .lock()
            .await
            .authorize_node(network, target, Some(NodeRole::Replica))?;
        let value = tokio::time::timeout(
            Duration::from_secs(5),
            self.hub.request(network, target, payload),
        )
        .await
        .context("history peer is currently unreachable")??;
        budget.response(&value)?;
        self.store
            .lock()
            .await
            .authorize_node(network, target, Some(NodeRole::Replica))?;
        if let Some(error) = value["error"].as_str() {
            bail!("{error}");
        }
        Ok(value)
    }
    async fn history_sources(&self, network: &str, preferred: &str) -> Result<Vec<HistorySource>> {
        let local = self.identity.node_id()?;
        let mut sources = vec![preferred.to_owned()];
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
            .map(|node_id| HistorySource {
                node_id,
                batch: None,
            })
            .collect())
    }
    async fn history_events(
        &self,
        network: &str,
        sources: &mut [HistorySource],
        hash: &str,
        budget: &mut HistoryBudget,
    ) -> Result<Vec<SignedEvent>> {
        let initial_events = budget.events;
        for source in sources {
            if budget.events == 0 || budget.bytes == 0 {
                break;
            }
            let target = &source.node_id;
            let before = budget.events;
            let result = async {
                let batch = match source.batch {
                    Some(batch) => batch,
                    None => {
                        let capabilities = self.history_response(network, target, json!({"type":"capabilities"}), budget).await?;
                        let batch = capabilities["history_batch"]["version"] == 1;
                        source.batch = Some(batch);
                        batch
                    }
                };
                let limit = budget.events.min(HISTORY_BATCH_EVENTS);
                let max_bytes = budget.bytes.min(HISTORY_BATCH_BYTES);
                let value = if batch {
                    self.history_response(network, target, json!({"type":"history_batch","hash":hash,"limit":limit,"max_bytes":max_bytes}), budget).await?
                } else {
                    Value::Null
                };
                let single = !batch || (value["version"] == 1 && value["hash"] == hash && value["single_event"] == true && value.get("events").is_none() && serde_json::to_vec(&value)?.len() <= max_bytes);
                let value = if single {
                    let value = self.history_response(network, target, json!({"type":"event","hash":hash}), budget).await?;
                    budget.events = budget.events.saturating_sub(1);
                    value
                } else {
                    budget.events = budget.events.saturating_sub(value["events"].as_array().map_or(1, Vec::len).clamp(1, limit));
                    value
                };
                let store = self.store.lock().await;
                store.authorize_node(network, target, Some(NodeRole::Replica))?;
                let envelopes = if single {
                    let envelope: SignedEvent = serde_json::from_value(value["event"].clone())?;
                    ensure!(envelope.hash == hash, "event response hash mismatch");
                    store.verify_event(network, &envelope)?;
                    vec![envelope]
                } else {
                    verify_history_batch(&store, network, hash, &value, limit, max_bytes)?
                };
                if !single {
                    tracing::trace!(network, target, hash, count=envelopes.len(), "history batch fetched");
                }
                for envelope in &envelopes {
                    tracing::trace!(network, target, hash=%envelope.hash, mode=if single {"single"} else {"batch"}, "history event fetched");
                }
                Ok(envelopes)
            }
            .await;
            match result {
                Ok(envelopes) => return Ok(envelopes),
                Err(error) => {
                    if budget.events == before {
                        budget.events -= 1;
                    }
                    tracing::debug!(network, target, error=%error, "history object source unavailable")
                }
            }
        }
        if budget.events == initial_events && budget.events != 0 {
            budget.events -= 1;
        }
        bail!("event unavailable from authorized replicas")
    }
    async fn sync_from(&self, network: &str, target: &str) -> Result<Vec<ClosureSync>> {
        self.store
            .lock()
            .await
            .authorize_node(network, target, Some(NodeRole::Replica))?;
        let inventory = self
            .read_peer(network, target, json!({"type":"inventory"}))
            .await?;
        let state: NetworkState = serde_json::from_value(inventory["membership"].clone())?;
        ensure!(
            state.genesis.network_id == network,
            "wrong membership network"
        );
        self.install_membership(state).await?;
        let scopes = inventory["scopes"]
            .as_array()
            .context("invalid inventory")?;
        let mut report = Vec::new();
        for scope in scopes {
            let scope_id = match field(scope, "scope_id") {
                Ok(scope) => scope,
                Err(error) => {
                    report.push(ClosureSync {
                        scope_id: "<invalid>".into(),
                        target: target.into(),
                        state: SyncState::Partial,
                        missing: MissingDependencies::default(),
                        processed: 0,
                        changed: false,
                        errors: vec![error.to_string()],
                    });
                    continue;
                }
            };
            let wanted: Result<Vec<String>> =
                serde_json::from_value(scope["heads"].clone()).map_err(Into::into);
            let result = match wanted {
                Ok(wanted) => self.fetch_scope(network, scope_id, target, &wanted).await,
                Err(error) => Err(error),
            };
            let outcome = result.unwrap_or_else(|error| ClosureSync {
                scope_id: scope_id.into(),
                target: target.into(),
                state: SyncState::Partial,
                missing: MissingDependencies::default(),
                processed: 0,
                changed: false,
                errors: vec![error.to_string()],
            });
            tracing::debug!(network, target, scope=scope_id, state=?outcome.state, processed=outcome.processed,
                changed=outcome.changed, missing_events=outcome.missing.events.len(), missing_blobs=outcome.missing.blobs.len(),
                conflicts=outcome.missing.conflicts.len(), "history scope sync finished");
            report.push(outcome);
        }
        Ok(report)
    }
    async fn fetch_closure(
        &self,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
    ) -> Result<ClosureSync> {
        self.fetch_closure_batch_mode(network, scope, target, wanted, 1024, false)
            .await
    }
    async fn fetch_scope(
        &self,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
    ) -> Result<ClosureSync> {
        self.fetch_closure_batch_mode(network, scope, target, wanted, 1024, true)
            .await
    }
    #[cfg(test)]
    async fn fetch_closure_batch(
        &self,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
        budget: usize,
    ) -> Result<ClosureSync> {
        self.fetch_closure_batch_mode(network, scope, target, wanted, budget, true)
            .await
    }
    async fn fetch_closure_batch_mode(
        &self,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
        budget: usize,
        persist: bool,
    ) -> Result<ClosureSync> {
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
        let mut sources = self.history_sources(network, target).await?;
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
            let progress =
                if let Some(progress) = previous.filter(|progress| progress.heads == roots) {
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
        let mut envelopes = Vec::new();
        let mut blobs: BTreeSet<BlobRef> = progress.blobs.iter().cloned().collect();
        let mut errors = Vec::new();
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
                    vec![envelope]
                }
                None => match self
                    .history_events(network, &mut sources, &hash, &mut work)
                    .await
                {
                    Ok(envelopes) => envelopes,
                    Err(error) => {
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
            for envelope in page {
                let hash = &envelope.hash;
                attempted.insert(hash.clone());
                queued.remove(hash);
                progress.event_cursor = Some(hash.clone());
                let event = self.store.lock().await.verify_event(network, &envelope)?;
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
                envelopes.push(envelope);
            }
        }
        queued.extend(unavailable);
        let mut missing = MissingDependencies {
            events: queued.into_iter().collect(),
            blobs: blobs.iter().cloned().collect(),
            conflicts: Vec::new(),
        };
        let inserted = {
            let mut store = self.store.lock().await;
            store.authorize_node(network, &self.identity.node_id()?, Some(NodeRole::Replica))?;
            let inserted = store.ingest_events(network, &envelopes)?;
            progress.advance(&missing);
            save_sync_progress(&mut store, network, &key, &progress, persist)?;
            inserted
        };
        let blob_page = rotated_page(
            &blobs.into_iter().collect::<Vec<_>>(),
            progress.blob_cursor.as_ref(),
            4,
        );
        let mut blobs_completed = 0;
        for blob in blob_page {
            progress.blob_cursor = Some(blob.clone());
            if self.store.lock().await.has_blob(network, &blob)? {
                missing.blobs.retain(|wanted| wanted != &blob);
                continue;
            }
            match self.history_blob(network, &sources, &blob).await {
                Ok(changed) => {
                    missing.blobs.retain(|wanted| wanted != &blob);
                    blobs_completed += usize::from(changed);
                }
                Err(error) => errors.push(error.to_string()),
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
            if missing.is_complete() || (envelopes.is_empty() && blobs_completed == 0) {
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
        } else if !missing.conflicts.is_empty() {
            SyncState::Partial
        } else if sources.is_empty()
            || (processed > 0 && !errors.is_empty() && envelopes.is_empty())
        {
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
        Ok(ClosureSync {
            scope_id: scope.into(),
            target: target.into(),
            state,
            missing,
            processed,
            changed,
            errors,
        })
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
        json!({"roots":self.config.roots,"executors":["codex","claude_code","cursor_agent"],
            "executor_capabilities":{
                "codex":{"actions":ExecutorKind::Codex.capabilities(),"verified":false},
                "claude_code":{"actions":ExecutorKind::ClaudeCode.capabilities(),"verified":false},
                "cursor_agent":{"actions":ExecutorKind::CursorAgent.capabilities(),"verified":false}},
            "manager":self.config.manager.as_ref().map(|m|m.provider.clone()),
            "manager_capabilities":manager.map(|kind|kind.capabilities()),"reliable_schedule":{"version":1},
            "history_batch":history_batch_capability()})
    }
    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let write = !matches!(
            input["method"].as_str(),
            Some(
                "nodes"
                    | "conversation"
                    | "conversations"
                    | "bots"
                    | "execution_status"
                    | "session"
                    | "operation"
                    | "notifications"
                    | "export"
            )
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
            .authorize_node(network, &self.identity.node_id()?, None)?;
        let method = field(&input, "method")?;
        if !access.human {
            ensure!(
                matches!(method, "nodes" | "create_task" | "update_task" | "operate"),
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
                let local = self.identity.node_id()?;
                let mut nodes = vec![];
                let view = bundle.project()?;
                let revoked_ids = view.revoked.clone();
                for member in view
                    .members
                    .into_iter()
                    .filter(|member| member.kind == IdentityKind::Node)
                {
                    let mut cert = member.certificate()?;
                    cert.revoked = view.revoked.contains(&cert.node_id);
                    let here = cert.node_id == local;
                    let live = here || online.contains(&cert.node_id);
                    let capabilities = if here {
                        self.capabilities()
                    } else if live && !cert.revoked {
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            self.peer_request(
                                network,
                                &cert.node_id,
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
                    nodes.push(json!({"node_id":cert.node_id,"roles":cert.roles,"revoked":cert.revoked,"local":here,"online":live&&!cert.revoked,"capabilities":capabilities}));
                }
                let history = bundle.historical_nodes()?;
                for revoked in revoked_ids {
                    if nodes.iter().any(|node| node["node_id"] == revoked) {
                        continue;
                    }
                    if history.iter().any(|node| node.subject_id == revoked) {
                        nodes.push(json!({"node_id":revoked,"roles":[],"revoked":true,"local":false,"online":false,"capabilities":Value::Null}));
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
            "task_control" => self.task_control(&access, &input).await,
            "execution_control" => self.execution_control(&access, &input).await,
            "execution_status" => self.execution_status(&access, &input).await,
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
            "rebind_execution" => self.rebind_execution(&access, &input).await,
            "operate" => {
                let request: OperationRequest = serde_json::from_value(input["request"].clone())?;
                self.require_scope(&access, &request.conversation_id)
                    .await?;
                self.dispatch(access, request).await
            }
            "session" => {
                self.session_fact(
                    network,
                    field(&input, "target_node")?,
                    field(&input, "session_id")?,
                )
                .await
            }
            "operation" => {
                let target = field(&input, "target_node")?;
                let op = field(&input, "operation_id")?;
                if target == self.identity.node_id()? {
                    Ok(json!({"operation":self.store.lock().await.operation(network,op)?}))
                } else {
                    self.peer_request(
                        network,
                        target,
                        json!({"type":"operation","operation_id":op}),
                    )
                    .await
                }
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
            "reconcile" => {
                ensure!(access.human, "reconciliation requires the user");
                self.reconcile(
                    network,
                    field(&input, "target_node")?,
                    field(&input, "operation_id")?,
                )
                .await
            }
            "resolve_unknown" => {
                ensure!(access.human, "resolution requires the user");
                self.resolve_unknown(network, &input).await
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
    async fn cache_context_bindings(&self, network: &str, scope: &str) -> Result<()> {
        let context = self.store.lock().await.context(network, scope, &[])?;
        let mut proofs = context
            .events
            .iter()
            .filter_map(|event| {
                if let EventBody::DispatchRecorded { action, .. } = &event.body {
                    serde_json::from_value::<OperationRequest>(action.clone())
                        .ok()
                        .and_then(|request| request.binding)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        proofs.sort_by_key(|proof| proof.binding.generation);
        let mut store = self.store.lock().await;
        for proof in proofs {
            store.install_historical_binding(&proof)?;
        }
        Ok(())
    }

    async fn confirm_binding(
        self: &Arc<Self>,
        binding: crate::binding::TaskBinding,
    ) -> Result<crate::binding::SignedBinding> {
        let proof = if binding.authority_node == self.identity.node_id()? {
            self.store
                .lock()
                .await
                .bind_task(&self.identity, &binding)?
        } else {
            let reply = self
                .peer_request(
                    &binding.network_id,
                    &binding.authority_node,
                    serde_json::json!({"type":"bind_task","binding":binding}),
                )
                .await
                .context("task binding authority is unreachable; execution was not reassigned")?;
            serde_json::from_value(reply["binding"].clone())?
        };
        self.store.lock().await.install_binding(&proof)?;
        Ok(proof)
    }

    async fn prepare_operation_binding(
        self: &Arc<Self>,
        request: &mut OperationRequest,
    ) -> Result<()> {
        self.cache_context_bindings(&request.network_id, &request.conversation_id)
            .await?;
        if let Some(proof) = &request.binding {
            self.store.lock().await.install_binding(proof)?;
            return self.store.lock().await.validate_operation_binding(request);
        }
        if request.task_id.is_none() {
            let context = self.store.lock().await.context(
                &request.network_id,
                &request.conversation_id,
                &[],
            )?;
            if let Some(proof) = context.events.iter().rev().find_map(|event| {
                if let EventBody::DispatchRecorded {
                    action,
                    target_node,
                    session_id,
                    ..
                } = &event.body
                    && target_node == &request.target_node
                    && session_id == &request.session_id
                {
                    serde_json::from_value::<OperationRequest>(action.clone())
                        .ok()
                        .and_then(|request| request.binding)
                } else {
                    None
                }
            }) {
                request.task_id = Some(proof.binding.task_id.clone());
                request.binding = Some(proof);
                return self.store.lock().await.validate_operation_binding(request);
            }
            return self.store.lock().await.validate_operation_binding(request);
        }
        let task_id = request
            .task_id
            .as_ref()
            .context("task binding requires a task")?;
        let (creation, event) = {
            let store = self.store.lock().await;
            let creation =
                store.task_creation(&request.network_id, &request.conversation_id, task_id)?;
            let event = store.verify_event(&request.network_id, &creation)?;
            (creation, event)
        };
        let mut existing = self
            .store
            .lock()
            .await
            .task_binding(&request.network_id, &creation.hash)?;
        if event.author != self.identity.node_id()?
            && existing.as_ref().is_none_or(|proof| {
                proof.binding.target_node != request.target_node
                    || Some(proof.binding.session_id.as_str()) != request.session_id.as_deref()
            })
            && let Ok(reply) = self
                .peer_request(
                    &request.network_id,
                    &event.author,
                    serde_json::json!({"type":"task_binding","task_ref":creation.hash}),
                )
                .await
            && !reply["binding"].is_null()
        {
            let proof: crate::binding::SignedBinding =
                serde_json::from_value(reply["binding"].clone())?;
            self.store.lock().await.install_binding(&proof)?;
            existing = Some(proof);
        }
        if let Some(proof) = existing {
            request.binding = Some(proof);
            return self.store.lock().await.validate_operation_binding(request);
        }
        let open_spec = if request.action["type"] == "open" {
            request.action.clone()
        } else {
            let value = self
                .session_fact(
                    &request.network_id,
                    &request.target_node,
                    request
                        .session_id
                        .as_deref()
                        .context("execution session is required")?,
                )
                .await?;
            serde_json::json!({"type":"open","executor":value["session"]["executor"],"cwd":value["session"]["cwd"]})
        };
        let binding = crate::binding::TaskBinding {
            network_id: request.network_id.clone(),
            conversation_id: request.conversation_id.clone(),
            task_ref: creation.hash,
            task_id: task_id.clone(),
            authority_node: event.author,
            generation: 1,
            target_node: request.target_node.clone(),
            session_id: request
                .session_id
                .clone()
                .context("task binding requires a session")?,
            open_spec,
            previous_binding_hash: None,
            previous_seal: None,
        };
        request.binding = Some(self.confirm_binding(binding).await?);
        self.store.lock().await.validate_operation_binding(request)
    }

    async fn rebind_execution(self: &Arc<Self>, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "moving execution requires the user");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        self.sync_all(&access.network_id).await?;
        ensure!(
            self.store
                .lock()
                .await
                .context(&access.network_id, scope, &[])?
                .missing
                .is_complete(),
            "task history is incomplete"
        );
        self.cache_context_bindings(&access.network_id, scope)
            .await?;
        let creation = self.store.lock().await.task_creation(
            &access.network_id,
            scope,
            field(input, "task_id")?,
        )?;
        let previous = self
            .store
            .lock()
            .await
            .task_binding(&access.network_id, &creation.hash)?
            .context("task has no confirmed execution binding")?;
        let old = &previous.binding;
        let seal = if old.target_node == self.identity.node_id()? {
            self.store
                .lock()
                .await
                .seal_binding(&self.identity, &previous)?
        } else {
            let reply = self
                .peer_request(
                    &access.network_id,
                    &old.target_node,
                    serde_json::json!({"type":"seal_binding","binding":previous,"human":true}),
                )
                .await
                .context("original executor is unreachable; task was not reassigned")?;
            serde_json::from_value(reply["seal"].clone())?
        };
        let next = crate::binding::TaskBinding {
            network_id: old.network_id.clone(),
            conversation_id: old.conversation_id.clone(),
            task_ref: old.task_ref.clone(),
            task_id: old.task_id.clone(),
            authority_node: old.authority_node.clone(),
            generation: old
                .generation
                .checked_add(1)
                .context("binding generation overflow")?,
            target_node: field(input, "target_node")?.into(),
            session_id: id(input, "session_id")?,
            open_spec: serde_json::json!({"type":"open","executor":field(input,"executor")?,"cwd":field(input,"cwd")?}),
            previous_binding_hash: Some(crate::binding::binding_hash(old)?),
            previous_seal: Some(seal),
        };
        Ok(serde_json::json!({"binding":self.confirm_binding(next).await?}))
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        access: Access,
        mut request: OperationRequest,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(
            request.network_id == access.network_id,
            "operation belongs to another network"
        );
        self.require_scope(&access, &request.conversation_id)
            .await?;
        id(&json!({"id":request.operation_id}), "id")?;
        if let Some(session) = &request.session_id {
            id(&json!({"id":session}), "id")?;
        }
        ensure!(
            (request.action["type"] == "command") == request.session_id.is_none(),
            "commands must be sessionless and SDK operations require a session"
        );
        ensure!(
            access.human || request.action["type"] != "respond_request",
            "only the user may answer approvals"
        );
        if !access.human && !matches!(request.action["type"].as_str(), Some("open" | "command")) {
            self.require_execution_scope(
                &access,
                &request.conversation_id,
                &request.target_node,
                request
                    .session_id
                    .as_deref()
                    .context("execution session is required")?,
            )
            .await?;
        }
        {
            let waiting = Instant::now();
            let store = self.store.lock().await;
            let waited = waiting.elapsed();
            let held = Instant::now();
            store.authorize_node(
                &access.network_id,
                &request.target_node,
                Some(NodeRole::Executor),
            )?;
            let known_gap = !request.basis_heads.is_empty()
                && store
                    .operation(&request.network_id, &request.operation_id)?
                    .is_none()
                && store.has_known_basis_gap(&request.network_id, &request.basis_heads)?;
            tracing::trace!(network=%request.network_id, scope=%request.conversation_id, stage="dispatch_basis", wait_ms=waited.as_millis() as u64,
                hold_ms=held.elapsed().as_millis() as u64, known_gap, "history store phase finished");
            ensure!(!known_gap, "operation basis is incomplete or conflicting");
        }
        if !access.human
            && matches!(
                request.action["type"].as_str(),
                Some("open" | "send" | "steer")
            )
            && let Some(task) = &request.task_id
        {
            require_task_write(
                &*self.store.lock().await,
                &request.network_id,
                &request.conversation_id,
                task,
                false,
            )?;
        }
        request.manager_authority =
            access
                .manager
                .as_ref()
                .map(|manager| ManagerDispatchAuthority {
                    scheduler_node: manager.target_node.clone(),
                    trigger: manager.trigger.clone(),
                    followup_version: manager.followup_version.unwrap_or(0),
                });
        self.prepare_operation_binding(&mut request).await?;
        Self::record_manager_operation(&mut *self.store.lock().await, &access, &request)?;
        let context = self.store.lock().await.context(
            &access.network_id,
            &request.conversation_id,
            &request.basis_heads,
        )?;
        ensure!(
            context.missing.is_complete(),
            "operation basis is incomplete or conflicting"
        );
        let old = context.events.iter().find_map(|e| {
            if let EventBody::DispatchRecorded {
                operation_id,
                action,
                ..
            } = &e.body
            {
                (operation_id == &request.operation_id).then_some((e, action))
            } else {
                None
            }
        });
        let head = if let Some((event, old)) = old {
            ensure!(
                old == &serde_json::to_value(&request)?,
                "operation ID reused with different request"
            );
            crypto::digest(&serde_json::to_vec(event)?)
        } else {
            if !access.human
                && matches!(
                    request.action["type"].as_str(),
                    Some("open" | "send" | "steer")
                )
                && let Some(task) = &request.task_id
            {
                require_task_write(
                    &*self.store.lock().await,
                    &request.network_id,
                    &request.conversation_id,
                    task,
                    false,
                )?;
            }
            let event = {
                let mut store = self.store.lock().await;
                Self::check_manager_permission(&store, &access, true, request.task_id.as_deref())?;
                store.append_event(
                    &self.identity,
                    &access.network_id,
                    &request.conversation_id,
                    EventBody::DispatchRecorded {
                        operation_id: request.operation_id.clone(),
                        task_id: request.task_id.clone(),
                        target_node: request.target_node.clone(),
                        session_id: request.session_id.clone(),
                        action: serde_json::to_value(&request)?,
                    },
                    vec![],
                    now_ms(),
                )?
            };
            self.publish_changed(&access.network_id, &request.conversation_id, &event.hash);
            event.hash
        };
        if request.target_node == self.identity.node_id()? {
            self.admit_operation_checked(request, access.human, Some(&access), None)
                .await
        } else {
            Self::check_manager_permission(
                &*self.store.lock().await,
                &access,
                true,
                request.task_id.as_deref(),
            )?;
            self.peer_request(&access.network_id,&request.target_node,json!({"type":"execute","request":request,"dispatch_head":head,"human":access.human})).await
        }
    }
    #[cfg(test)]
    async fn admit_operation(
        self: &Arc<Self>,
        request: OperationRequest,
        human: bool,
    ) -> Result<Value> {
        self.admit_operation_checked(request, human, None, None)
            .await
    }
    async fn admit_operation_checked(
        self: &Arc<Self>,
        mut request: OperationRequest,
        human: bool,
        manager_access: Option<&Access>,
        peer_origin: Option<&str>,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        self.require_role(&request.network_id, NodeRole::Executor)
            .await?;
        ensure!(
            request.target_node == self.identity.node_id()?,
            "wrong execution target"
        );
        let kind = field(&request.action, "type")?;
        ensure!(
            human || kind != "respond_request",
            "only the user may answer approvals"
        );
        let mut store = self.store.lock().await;
        if let Some(origin) = peer_origin {
            let sender = store.authorize_node(&request.network_id, origin, None)?;
            ensure!(
                sender.roles.contains(&NodeRole::Scheduler)
                    || sender.roles.contains(&NodeRole::Gateway),
                "origin cannot dispatch"
            );
            ensure!(
                !human || sender.roles.contains(&NodeRole::Gateway),
                "origin cannot assert human authority"
            );
        }
        if let Some(access) = manager_access {
            Self::check_manager_permission(&store, access, true, request.task_id.as_deref())?;
        }
        let existing_operation = store.operation(&request.network_id, &request.operation_id)?;
        if existing_operation.is_none() && !request.basis_heads.is_empty() {
            ensure!(
                !store.has_known_basis_gap(&request.network_id, &request.basis_heads)?,
                "required context is incomplete or conflicting"
            );
        }
        if kind == "command" {
            ensure!(
                request.session_id.is_none()
                    && request.task_id.is_none()
                    && request.binding.is_none(),
                "commands cannot bind an SDK session or task"
            );
            let spec = self.command_spec(&request.action)?;
            let cwd = self.allowed_path(
                spec.cwd
                    .to_str()
                    .context("command directory must be UTF-8")?,
            )?;
            let mut resources = vec![
                cwd.to_str()
                    .context("command directory must be UTF-8")?
                    .to_owned(),
            ];
            for resource in &request.resources {
                resources.push(
                    self.allowed_path(resource)?
                        .to_str()
                        .context("resource must be UTF-8")?
                        .to_owned(),
                );
            }
            resources.sort();
            resources.dedup();
            request.resources = resources;
            if let Some(old) = existing_operation {
                ensure!(
                    old.request == request,
                    "operation ID reused with different body"
                );
                return Ok(json!({"operation":old}));
            }
            let operation = store.accept_operation(&request.network_id, &request)?;
            drop(store);
            let node = self.clone();
            tokio::spawn(async move {
                if let Err(error) = node.execute_command(request).await {
                    tracing::warn!(%error,"command requires reconciliation");
                }
            });
            return Ok(json!({"operation":operation}));
        }
        let current = store.session(
            &request.network_id,
            &request.target_node,
            request
                .session_id
                .as_deref()
                .context("execution session is required")?,
        )?;
        if !human && existing_operation.is_none() && matches!(kind, "open" | "send" | "steer") {
            if let Some(task) = &request.task_id {
                require_task_write(
                    &store,
                    &request.network_id,
                    &request.conversation_id,
                    task,
                    false,
                )?;
            } else {
                let previous: Vec<_> = store
                    .operations(&request.network_id)?
                    .into_iter()
                    .map(|operation| operation.request)
                    .filter(|operation| {
                        operation.target_node == request.target_node
                            && operation.session_id == request.session_id
                            && matches!(
                                operation.action["type"].as_str(),
                                Some("open" | "send" | "steer")
                            )
                    })
                    .collect();
                let latest_revision = previous
                    .iter()
                    .map(|operation| operation.expected_revision)
                    .max();
                ensure!(
                    !previous.iter().any(|operation| {
                        Some(operation.expected_revision) == latest_revision
                            && operation.task_id.is_some()
                    }),
                    "task_id is required for new agent work in a task-associated execution session"
                );
            }
        }
        let session = if kind == "open" {
            ensure!(
                current.is_none() || existing_operation.is_some(),
                "execution session already exists"
            );
            let executor: ExecutorKind =
                serde_json::from_value(request.action["executor"].clone())?;
            let cwd = self.allowed_path(field(&request.action, "cwd")?)?;
            current.unwrap_or(ExecutionSession {
                network_id: request.network_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request
                    .session_id
                    .clone()
                    .context("SDK operation requires a session")?,
                revision: 0,
                state: "opening".into(),
                executor: serde_json::to_value(executor)?.as_str().unwrap().into(),
                cwd: cwd.to_str().context("directory must be UTF-8")?.into(),
                native_session_id: None,
            })
        } else {
            current.context("execution session is not open")?
        };
        let action = self.engine_action(&request.action)?;
        action.validate()?;
        let control = !matches!(action, EngineAction::Open {} | EngineAction::Send { .. });
        if control {
            ensure!(
                request.resources.is_empty(),
                "control operations cannot reserve resources"
            );
        } else {
            let path = self.allowed_path(&session.cwd)?;
            let mut resources = vec![path.to_str().context("directory must be UTF-8")?.to_owned()];
            for resource in &request.resources {
                resources.push(
                    self.allowed_path(resource)?
                        .to_str()
                        .context("resource must be UTF-8")?
                        .to_owned(),
                );
            }
            resources.sort();
            resources.dedup();
            request.resources = resources;
        }
        if let Some(old) = existing_operation {
            ensure!(
                old.request == request,
                "operation ID reused with different body"
            );
            return Ok(json!({"operation":old}));
        }
        let operation = if control {
            store.accept_control_operation(&request.network_id, &request)?
        } else {
            store.accept_operation(&request.network_id, &request)?
        };
        if kind == "open" {
            store.upsert_session(&request.network_id, &session)?;
        }
        drop(store);
        let node = self.clone();
        tokio::spawn(async move {
            if let Err(e) = node.execute_operation(request).await {
                tracing::warn!(error=%e,"operation execution requires reconciliation");
            }
        });
        Ok(json!({"operation":operation}))
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
    async fn require_execution_scope(
        &self,
        access: &Access,
        scope: &str,
        target: &str,
        session: &str,
    ) -> Result<()> {
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
        ensure!(context.events.iter().any(|e| e.scope_id==scope && matches!(&e.body,EventBody::DispatchRecorded{target_node,session_id,..} if target_node==target && session_id.as_deref()==Some(session))),"execution session is outside this conversation");
        Ok(())
    }
    fn engine_action(&self, action: &Value) -> Result<EngineAction> {
        if action["type"] == "open" {
            let map = action.as_object().context("invalid action")?;
            ensure!(
                map.keys()
                    .all(|k| matches!(k.as_str(), "type" | "executor" | "cwd")),
                "unexpected open field"
            );
            Ok(EngineAction::Open {})
        } else {
            Ok(serde_json::from_value(action.clone())?)
        }
    }
    async fn execute_operation(self: Arc<Self>, request: OperationRequest) -> Result<()> {
        let _work = self.maintenance.enter()?;
        if request.action["type"] == "command" {
            return self.execute_command(request).await;
        }
        let action = self.engine_action(&request.action)?;
        let preparation: Result<ExecutionSession> = async {
            self.require_role(&request.network_id, NodeRole::Executor)
                .await?;
            let mut store = self.store.lock().await;
            let session = store
                .session(
                    &request.network_id,
                    &request.target_node,
                    request
                        .session_id
                        .as_deref()
                        .context("execution session is required")?,
                )?
                .context("accepted operation has no local session")?;
            store.validate_operation_binding(&request)?;
            self.allowed_path(&session.cwd)?;
            for resource in &request.resources {
                self.allowed_path(resource)?;
            }
            store.claim_operation(&request.network_id, &request.operation_id)?;
            Ok(session)
        }
        .await;
        let session = match preparation {
            Ok(session) => session,
            Err(error) => {
                let accepted = self
                    .store
                    .lock()
                    .await
                    .operation(&request.network_id, &request.operation_id)?
                    .is_some_and(|o| o.state == OperationState::Accepted);
                if accepted {
                    self.finish(
                        &request,
                        OperationState::Rejected,
                        json!({"error":format!("{error:#}")}),
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        let native = ExecutorSession {
            executor: serde_json::from_value(json!(session.executor))?,
            cwd: PathBuf::from(&session.cwd),
            native_session_id: session.native_session_id.clone(),
        };
        let store = self.store.clone();
        let mut persisted = session.clone();
        let network = request.network_id.clone();
        let result = self
            .engine
            .execute(&native, action.clone(), move |id| async move {
                persisted.native_session_id = Some(id);
                store.lock().await.upsert_session(&network, &persisted)?;
                Ok(())
            })
            .await;
        match result {
            Ok(result) => {
                let mut store = self.store.lock().await;
                if store
                    .operation(&request.network_id, &request.operation_id)?
                    .is_some_and(|o| o.state.terminal())
                {
                    return Ok(());
                }
                let mut session = store
                    .session(
                        &request.network_id,
                        &request.target_node,
                        request
                            .session_id
                            .as_deref()
                            .context("execution session is required")?,
                    )?
                    .context("session disappeared")?;
                session.native_session_id = result.native_session_id;
                if native.executor == ExecutorKind::Codex
                    && matches!(action, EngineAction::Send { .. })
                {
                    session.state = "running".into();
                    store.upsert_session(&request.network_id, &session)?;
                    store.local_set(
                        &request.network_id,
                        &format!("turn:{}", request.operation_id),
                        &result.result,
                    )?;
                    drop(store);
                    self.append(
                        &request.network_id,
                        &request.conversation_id,
                        EventBody::ExecutionObserved {
                            operation_id: request.operation_id.clone(),
                            task_id: request.task_id.clone(),
                            target_node: request.target_node.clone(),
                            session_id: request.session_id.clone(),
                            status: "running".into(),
                            detail: result.result,
                        },
                    )
                    .await?;
                    self.reconcile(
                        &request.network_id,
                        &request.target_node,
                        &request.operation_id,
                    )
                    .await?;
                } else {
                    store.upsert_session(&request.network_id, &session)?;
                    drop(store);
                    self.finish(&request, OperationState::Succeeded, result.result)
                        .await?;
                }
            }
            Err(error) => {
                {
                    let mut store = self.store.lock().await;
                    let mut session = store
                        .session(
                            &request.network_id,
                            &request.target_node,
                            request
                                .session_id
                                .as_deref()
                                .context("execution session is required")?,
                        )?
                        .context("session disappeared")?;
                    session.native_session_id = error.native_session_id.clone();
                    store.upsert_session(&request.network_id, &session)?;
                }
                let state = if error.uncertain {
                    OperationState::Unknown
                } else {
                    OperationState::Failed
                };
                self.finish(
                    &request,
                    state,
                    json!({"error":error.message,"native_session_id":error.native_session_id}),
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn finish(
        self: &Arc<Self>,
        request: &OperationRequest,
        state: OperationState,
        result: Value,
    ) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let _completion = self.completion_lock.lock().await;
        let status = serde_json::to_value(&state)?.as_str().unwrap().to_owned();
        let completion_key = format!("completion:{}", request.operation_id);
        {
            let mut store = self.store.lock().await;
            if let Some(old) = store.operation(&request.network_id, &request.operation_id)?
                && old.state.terminal()
                && (old.state != state || old.result != result)
            {
                return Ok(());
            }
            store.local_set(
                &request.network_id,
                &completion_key,
                &json!({"state":state,"result":result}),
            )?;
            store.finish_operation(
                &request.network_id,
                &request.operation_id,
                state.clone(),
                result.clone(),
            )?;
            if let Some(mut session) = request
                .session_id
                .as_deref()
                .map(|session| store.session(&request.network_id, &request.target_node, session))
                .transpose()?
                .flatten()
            {
                let outstanding = store
                    .operations(&request.network_id)?
                    .into_iter()
                    .filter(|o| {
                        o.request.target_node == request.target_node
                            && o.request.session_id == request.session_id
                            && !o.state.terminal()
                    })
                    .collect::<Vec<_>>();
                session.state = if outstanding
                    .iter()
                    .any(|o| o.state == OperationState::Unknown)
                {
                    "unknown"
                } else if !outstanding.is_empty() {
                    "running"
                } else {
                    "idle"
                }
                .into();
                store.upsert_session(&request.network_id, &session)?;
            }
        }
        let mut blobs = vec![];
        let mut detail = result;
        let bytes = serde_json::to_vec(&detail)?;
        if bytes.len() > 262144 {
            let blob = BlobRef {
                hash: crypto::digest(&bytes),
                size: bytes.len() as u64,
            };
            self.store
                .lock()
                .await
                .put_blob(&request.network_id, &blob, &bytes)?;
            detail = json!({"result_blob":blob});
            blobs.push(blob);
        }
        let existing=self.store.lock().await.context(&request.network_id,&request.conversation_id,&[])?.events.into_iter().find(|e|matches!(&e.body,EventBody::ExecutionObserved{operation_id,status:s,..} if operation_id==&request.operation_id && s==&status));
        let event_hash = if let Some(existing) = existing {
            crypto::digest(&serde_json::to_vec(&existing)?)
        } else {
            self.append_blobs(
                &request.network_id,
                &request.conversation_id,
                EventBody::ExecutionObserved {
                    operation_id: request.operation_id.clone(),
                    task_id: request.task_id.clone(),
                    target_node: request.target_node.clone(),
                    session_id: request.session_id.clone(),
                    status: status.clone(),
                    detail,
                },
                blobs,
            )
            .await?
            .hash
        };
        if !matches!(request.action["type"].as_str(), Some("read")) {
            let notification_id = format!("result:{}:{}", request.operation_id, status);
            let already=self.store.lock().await.context(&request.network_id,&request.conversation_id,&[])?.events.iter().any(|e|matches!(&e.body,EventBody::NotificationRaised{notification_id:id,..} if id==&notification_id));
            if !already {
                self.append(
                    &request.network_id,
                    &request.conversation_id,
                    EventBody::NotificationRaised {
                        notification_id,
                        kind: status.clone(),
                        text: match state {
                            OperationState::Succeeded => "后台工作已完成。",
                            OperationState::Unknown => "后台工作的结果仍需核对。",
                            _ => "后台工作未完成，请查看详情。",
                        }
                        .into(),
                    },
                )
                .await?;
            }
            if state.terminal() && !self.maybe_followup(request, &event_hash).await? {
                return Ok(());
            }
        }
        if state.terminal() {
            self.progress_sequences
                .lock()
                .expect("progress sequence lock")
                .remove(&(request.network_id.clone(), request.operation_id.clone()));
        }
        self.store
            .lock()
            .await
            .local_delete(&request.network_id, &completion_key)?;
        Ok(())
    }
    async fn flush_completions(self: &Arc<Self>, network: &str) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let operations = self.store.lock().await.operations(network)?;
        for operation in operations {
            let key = format!("completion:{}", operation.request.operation_id);
            let pending = self.store.lock().await.local_value(network, &key)?;
            if let Some(value) = pending {
                self.finish(
                    &operation.request,
                    serde_json::from_value(value["state"].clone())?,
                    value["result"].clone(),
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn session_fact(&self, network: &str, target: &str, session_id: &str) -> Result<Value> {
        if target != self.identity.node_id()? {
            return self
                .read_peer(
                    network,
                    target,
                    json!({"type":"session","session_id":session_id}),
                )
                .await;
        }
        let session = self
            .store
            .lock()
            .await
            .session(network, target, session_id)?
            .context("execution session not found")?;
        let mut snapshot = self.engine.snapshot().await;
        if let Some(requests) = snapshot["pending_requests"].as_array_mut() {
            requests.retain(|r| {
                r["params"]["threadId"].as_str() == session.native_session_id.as_deref()
            });
        }
        let native = ExecutorSession {
            executor: serde_json::from_value(json!(session.executor))?,
            cwd: session.cwd.clone().into(),
            native_session_id: session.native_session_id.clone(),
        };
        let read = self
            .engine
            .execute(&native, EngineAction::Read {}, |_| async {
                bail!("read must not create a session")
            })
            .await;
        let last_turn = session
            .native_session_id
            .as_deref()
            .map(|id| snapshot["threads"][id]["last_turn"].clone())
            .unwrap_or(Value::Null);
        let actual = match read {
            Ok(r) => {
                json!({"thread":r.result,"last_turn":last_turn,"pending_requests":snapshot["pending_requests"]})
            }
            Err(e) => {
                json!({"error":e.message,"uncertain":e.uncertain,"last_turn":last_turn,"pending_requests":snapshot["pending_requests"]})
            }
        };
        Ok(json!({"session":session,"actual":actual}))
    }
    pub async fn current_facts(&self, network: &str, scope: &str) -> Result<Value> {
        let context = self.store.lock().await.context(network, scope, &[])?;
        let terminal: BTreeSet<_> = context
            .events
            .iter()
            .filter_map(|event| {
                if event.scope_id == scope
                    && let EventBody::ExecutionObserved {
                        operation_id,
                        target_node,
                        status,
                        ..
                    } = &event.body
                    && event.author == *target_node
                    && matches!(
                        status.as_str(),
                        "succeeded" | "failed" | "rejected" | "completed" | "interrupted"
                    )
                {
                    Some(operation_id.as_str())
                } else {
                    None
                }
            })
            .collect();
        let mut keys = BTreeSet::new();
        let mut operations = BTreeSet::new();
        for event in &context.events {
            if event.scope_id != scope {
                continue;
            }
            if let EventBody::DispatchRecorded {
                target_node,
                session_id,
                operation_id,
                ..
            } = &event.body
                && !terminal.contains(operation_id.as_str())
            {
                if let Some(session_id) = session_id {
                    keys.insert((target_node.clone(), session_id.clone()));
                }
                operations.insert((target_node.clone(), operation_id.clone()));
            }
        }
        let mut sessions = vec![];
        let mut facts = vec![];
        let mut records = vec![];
        for (target, session) in keys {
            match tokio::time::timeout(Duration::from_secs(3),self.session_fact(network,&target,&session)).await {Ok(Ok(value))=>{sessions.push(value["session"].clone());facts.push(value);},_=>facts.push(json!({"target_node":target,"session_id":session,"state":"unreachable","freshness":"unknown"}))}
        }
        for (target, op) in operations {
            if target == self.identity.node_id()? {
                if let Some(record) = self.store.lock().await.operation(network, &op)? {
                    records.push(json!(record));
                }
            } else if let Ok(Ok(value)) = tokio::time::timeout(
                Duration::from_secs(3),
                self.peer_request(
                    network,
                    &target,
                    json!({"type":"operation","operation_id":op}),
                ),
            )
            .await
                && !value["operation"].is_null()
            {
                records.push(value["operation"].clone());
            }
        }
        Ok(json!({"sessions":sessions,"operations":records,"facts":facts}))
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
                | "operation"
                | "task_binding"
                | "progress_read"
                | "chat_task_status"
                | "chat_execution_status"
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
                    .scopes(network)?
                    .into_iter()
                    .map(|s| Ok(json!({"scope_id":s,"heads":store.heads(network,&s)?})))
                    .collect::<Result<Vec<_>>>()?;
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
                let view = self.identities.public_state()?.project()?;
                ensure!(view.network_id == *network, "wrong personal network");
                view.authorize_node(&self.identity.node_id()?, Some(NodeRole::Replica))?;
                let (size, bytes) = store
                    .blob_range(
                        network,
                        &self.identity.node_id()?,
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
            "task_binding" => Ok(
                json!({"binding":self.store.lock().await.task_binding(network,field(input,"task_ref")?)?}),
            ),
            "bind_task" => {
                let sender =
                    self.store
                        .lock()
                        .await
                        .authorize_node(network, &message.origin, None)?;
                ensure!(
                    sender.roles.contains(&NodeRole::Scheduler)
                        || sender.roles.contains(&NodeRole::Gateway),
                    "origin cannot propose execution binding"
                );
                let binding: crate::binding::TaskBinding =
                    serde_json::from_value(input["binding"].clone())?;
                ensure!(binding.network_id == *network, "wrong binding network");
                Ok(json!({"binding":self.store.lock().await.bind_task(&self.identity,&binding)?}))
            }
            "seal_binding" => {
                let sender = self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                ensure!(
                    input["human"] == true && sender.roles.contains(&NodeRole::Gateway),
                    "sealing execution requires the user"
                );
                let proof: crate::binding::SignedBinding =
                    serde_json::from_value(input["binding"].clone())?;
                ensure!(
                    proof.binding.network_id == *network,
                    "wrong binding network"
                );
                Ok(json!({"seal":self.store.lock().await.seal_binding(&self.identity,&proof)?}))
            }
            "execute" => {
                let sender =
                    self.store
                        .lock()
                        .await
                        .authorize_node(network, &message.origin, None)?;
                ensure!(
                    sender.roles.contains(&NodeRole::Scheduler)
                        || sender.roles.contains(&NodeRole::Gateway),
                    "origin cannot dispatch"
                );
                let human = input["human"].as_bool().unwrap_or(false);
                ensure!(
                    !human || sender.roles.contains(&NodeRole::Gateway),
                    "origin cannot assert human authority"
                );
                let request: OperationRequest = serde_json::from_value(input["request"].clone())?;
                ensure!(request.network_id == *network, "wrong operation network");
                // Admission holds the complete dispatched history before any external call.
                let mut wanted = request.basis_heads.clone();
                wanted.push(field(input, "dispatch_head")?.into());
                self.fetch_closure(network, &request.conversation_id, &message.origin, &wanted)
                    .await?
                    .require_complete()?;
                ensure!(
                    self.store
                        .lock()
                        .await
                        .missing_dependencies(network, &wanted)?
                        .is_complete(),
                    "dispatch context is incomplete"
                );
                {
                    let store = self.store.lock().await;
                    let envelope = store
                        .event(network, field(input, "dispatch_head")?)?
                        .context("dispatch event is missing")?;
                    let event = store.verify_event(network, &envelope)?;
                    ensure!(
                        event.author == message.origin
                            && event.scope_id == request.conversation_id
                            && matches!(&event.body, EventBody::DispatchRecorded { operation_id, task_id, target_node, session_id, action }
                            if operation_id == &request.operation_id && task_id == &request.task_id && target_node == &request.target_node && session_id == &request.session_id && action == &serde_json::to_value(&request)?),
                        "execution request does not match its original signed dispatch"
                    );
                    if let Some(authority) = &request.manager_authority {
                        ensure!(
                            !human && authority.scheduler_node == message.origin,
                            "invalid autonomous dispatch authority"
                        );
                    }
                }
                self.cache_context_bindings(network, &request.conversation_id)
                    .await?;
                if let Some(proof) = &request.binding {
                    self.store.lock().await.install_binding(proof)?;
                }
                self.admit_operation_checked(request, human, None, Some(&message.origin))
                    .await
            }
            "session" => {
                self.session_fact(
                    network,
                    &self.identity.node_id()?,
                    field(input, "session_id")?,
                )
                .await
            }
            "operation" => Ok(
                json!({"operation":self.store.lock().await.operation(network,field(input,"operation_id")?)?}),
            ),
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
            "chat_task_status" | "chat_execution_status" => {
                let sender =
                    self.store
                        .lock()
                        .await
                        .authorize_node(network, &message.origin, None)?;
                ensure!(
                    sender.roles.contains(&NodeRole::Gateway)
                        || sender.roles.contains(&NodeRole::Scheduler),
                    "origin cannot observe execution controls"
                );
                if kind == "chat_execution_status" {
                    self.chat_execution_status(
                        network,
                        field(input, "conversation_id")?,
                        field(input, "operation_id")?,
                    )
                    .await
                } else {
                    self.chat_task_status(
                        network,
                        field(input, "conversation_id")?,
                        field(input, "task_id")?,
                    )
                    .await
                }
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
            "reconcile" => {
                self.reconcile(
                    network,
                    &self.identity.node_id()?,
                    field(input, "operation_id")?,
                )
                .await
            }
            "resolve_unknown" => {
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                self.resolve_unknown(network, input).await
            }
            _ => bail!("unknown peer message"),
        }
    }
    async fn observe_engine(self: &Arc<Self>, event: Value) -> Result<()> {
        let _work = self.maintenance.enter_when_open().await;
        let cli_progress = event["kind"] == "executor_progress";
        let method = event["method"].as_str().unwrap_or(if cli_progress {
            "executor/progress"
        } else {
            ""
        });
        let thread = event["params"]["threadId"]
            .as_str()
            .or_else(|| event["params"]["thread_id"].as_str())
            .or_else(|| event["params"]["session_id"].as_str())
            .or_else(|| event["native_session_id"].as_str());
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            let operations = self.store.lock().await.operations(&network)?;
            for operation in operations.into_iter().filter(|o| {
                matches!(o.state, OperationState::Running | OperationState::Unknown)
                    && o.request.action["type"] == "send"
            }) {
                let request = &operation.request;
                let session = self
                    .store
                    .lock()
                    .await
                    .session(
                        &network,
                        &request.target_node,
                        request
                            .session_id
                            .as_deref()
                            .context("execution session is required")?,
                    )?
                    .context("running operation lost session")?;
                if method == "bridge.codex_disconnected"
                    && session.executor == "codex"
                    && operation.state == OperationState::Running
                {
                    self.finish(
                        request,
                        OperationState::Unknown,
                        json!({"error":"Codex disconnected; reconcile before any retry"}),
                    )
                    .await?;
                    continue;
                }
                if thread.is_none() || thread != session.native_session_id.as_deref() {
                    continue;
                }
                if cli_progress && event["executor"] != session.executor {
                    continue;
                }
                let sequence = {
                    let mut sequences = self
                        .progress_sequences
                        .lock()
                        .expect("progress sequence lock");
                    let sequence = sequences
                        .entry((network.clone(), request.operation_id.clone()))
                        .or_default();
                    *sequence += 1;
                    *sequence
                };
                let turn = self
                    .store
                    .lock()
                    .await
                    .local_value(&network, &format!("turn:{}", request.operation_id))?;
                self.publish_progress(&network, json!({"type":"execution_progress","conversation_id":request.conversation_id,
                    "task_id":request.task_id,"task_ref":request.binding.as_ref().map(|proof| &proof.binding.task_ref),
                    "target_node":request.target_node,"session_id":request.session_id,"operation_id":request.operation_id,
                    "turn_id":turn.as_ref().and_then(|value|value["turn"]["id"].as_str()),
                    "source":self.identity.node_id()?,"source_node":self.identity.node_id()?,"sequence":sequence,"progress":observation::execution_progress(&event)}));
                if method == "turn/completed" {
                    let turn = field(&event["params"]["turn"], "id")?;
                    let known = self
                        .store
                        .lock()
                        .await
                        .local_value(&network, &format!("turn:{}", request.operation_id))?;
                    if known.as_ref().and_then(|v| v["turn"]["id"].as_str()) != Some(turn) {
                        continue;
                    }
                    let state = if event["params"]["turn"]["status"] == "completed" {
                        OperationState::Succeeded
                    } else {
                        OperationState::Failed
                    };
                    self.finish(request, state, event.clone()).await?;
                } else if !method.ends_with("/delta")
                    && (method == "item/completed"
                        || event.get("id").is_some()
                        || (cli_progress && event["event"]["type"] != "stream_event"))
                {
                    let bytes = serde_json::to_vec(&event)?;
                    let request_digest = crypto::digest(&bytes);
                    let request_notification =
                        format!("approval:{}:{}", request.operation_id, request_digest);
                    if event.get("id").is_some() {
                        let context = self.store.lock().await.context(
                            &network,
                            &request.conversation_id,
                            &[],
                        )?;
                        let observed = context.events.iter().any(|existing| {
                            matches!(&existing.body,
                            EventBody::ExecutionObserved { operation_id, status, detail, .. }
                            if operation_id == &request.operation_id && status == "needs_input"
                            && (detail == &event || detail["event_blob"]["hash"] == request_digest))
                        });
                        let notified = context.events.iter().any(|existing| matches!(&existing.body,
                            EventBody::NotificationRaised { notification_id, .. } if notification_id == &request_notification));
                        if observed && notified {
                            continue;
                        }
                    }
                    let mut blobs = vec![];
                    let detail = if bytes.len() > 262144 {
                        let blob = BlobRef {
                            hash: crypto::digest(&bytes),
                            size: bytes.len() as u64,
                        };
                        self.store.lock().await.put_blob(&network, &blob, &bytes)?;
                        blobs.push(blob.clone());
                        json!({"event_blob":blob})
                    } else {
                        event.clone()
                    };
                    self.append_blobs(
                        &network,
                        &request.conversation_id,
                        EventBody::ExecutionObserved {
                            operation_id: request.operation_id.clone(),
                            task_id: request.task_id.clone(),
                            target_node: request.target_node.clone(),
                            session_id: request.session_id.clone(),
                            status: if event.get("id").is_some() {
                                "needs_input"
                            } else {
                                "progress"
                            }
                            .into(),
                            detail,
                        },
                        blobs,
                    )
                    .await?;
                    if event.get("id").is_some() {
                        let exists = self.store.lock().await.context(&network, &request.conversation_id, &[])?.events.iter().any(|existing|
                            matches!(&existing.body, EventBody::NotificationRaised { notification_id, .. } if notification_id == &request_notification));
                        if !exists {
                            self.append(
                                &network,
                                &request.conversation_id,
                                EventBody::NotificationRaised {
                                    notification_id: request_notification,
                                    kind: "needs_input".into(),
                                    text: "助手正在等待本人审批或补充输入。".into(),
                                },
                            )
                            .await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    async fn reconcile(
        self: &Arc<Self>,
        network: &str,
        target: &str,
        operation_id: &str,
    ) -> Result<Value> {
        if target != self.identity.node_id()? {
            return self
                .peer_request(
                    network,
                    target,
                    json!({"type":"reconcile","operation_id":operation_id}),
                )
                .await;
        }
        let Some(operation) = self.store.lock().await.operation(network, operation_id)? else {
            return Ok(
                json!({"operation":null,"state":"not_found","target_node":target,"checked_at_ms":now_ms()}),
            );
        };
        if operation.state.terminal() {
            return Ok(json!({"operation":operation}));
        }
        let Some(session_id) = operation.request.session_id.as_deref() else {
            return Ok(
                json!({"operation":operation,"actual":{"state":"unknown","detail":"Command process outcome requires evidence from the execution device; it will not be replayed."}}),
            );
        };
        let mut actual = self.session_fact(network, target, session_id).await?;
        let known = self
            .store
            .lock()
            .await
            .local_value(network, &format!("turn:{operation_id}"))?;
        if let Some(turn_id) = known.as_ref().and_then(|v| v["turn"]["id"].as_str()) {
            let native_id = actual["session"]["native_session_id"]
                .as_str()
                .context("execution session has no native ID")?;
            let turn = match self.engine.read_turn(native_id, turn_id).await {
                Ok(turn) => turn,
                Err(error) => {
                    actual["reconcile_error"] = json!(format!("{error:#}"));
                    None
                }
            };
            if let Some(turn) = turn
                && matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            {
                self.finish(
                    &operation.request,
                    if turn["status"] == "completed" {
                        OperationState::Succeeded
                    } else {
                        OperationState::Failed
                    },
                    json!({"reconciled_turn":turn}),
                )
                .await?;
            }
        }
        Ok(
            json!({"operation":self.store.lock().await.operation(network,operation_id)?,"actual":actual}),
        )
    }
    async fn resolve_unknown(self: &Arc<Self>, network: &str, input: &Value) -> Result<Value> {
        let target = field(input, "target_node")?;
        if target != self.identity.node_id()? {
            let mut value = input.clone();
            value["type"] = json!("resolve_unknown");
            return self.peer_request(network, target, value).await;
        }
        let operation = self
            .store
            .lock()
            .await
            .operation(network, field(input, "operation_id")?)?
            .context("operation not found")?;
        ensure!(
            operation.state == OperationState::Unknown,
            "only unknown outcomes require a human resolution"
        );
        let evidence = field(input, "evidence")?;
        ensure!(
            evidence.len() >= 12,
            "describe how you verified the process and external effects"
        );
        let state = match field(input, "outcome")? {
            "succeeded" => OperationState::Succeeded,
            "failed" => OperationState::Failed,
            _ => bail!("invalid resolution"),
        };
        self.finish(
            &operation.request,
            state,
            json!({"human_verified":true,"evidence":evidence}),
        )
        .await?;
        Ok(
            json!({"operation":self.store.lock().await.operation(network,&operation.request.operation_id)?}),
        )
    }
}
