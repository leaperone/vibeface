use super::*;
use crate::config;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::{initialize_node, signed_main_input};

pub(super) async fn node(identity: &LocalIdentity) -> Result<(Arc<Runtime>, PathBuf)> {
    node_with(
        identity,
        net::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            ..Default::default()
        },
    )
    .await
}

pub(super) async fn node_with(
    identity: &LocalIdentity,
    network: net::NetworkConfig,
) -> Result<(Arc<Runtime>, PathBuf)> {
    let dir = std::env::temp_dir().join(format!("vibeface-peers-{}", uuid::Uuid::new_v4()));
    config::private_dir(&dir)?;
    let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
    configuration.network = network;
    configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
    initialize_node(&dir, &configuration, identity)?;
    Ok((Arc::new(Runtime::open(&dir).await?), dir))
}

pub(super) async fn ready_member_rpc(node: &Runtime, network: &str, peer: &str) -> Result<()> {
    // SYNC/BLOB traffic does not establish the separately cached RPC connection.
    // An empty address exchange only reads member metadata and does not sync inputs.
    let reply = node
        .peer_request(
            network,
            peer,
            AddressExchange { records: vec![] }.into_request(network)?,
        )
        .await
        .context("member RPC is ready before timing delivery")?;
    ensure!(
        reply["records"].is_array(),
        "member RPC returns address records"
    );
    Ok(())
}

pub(super) async fn finish_nodes(
    result: Result<()>,
    nodes: Vec<(Arc<Runtime>, PathBuf)>,
) -> Result<()> {
    match (result, stop_nodes(nodes).await) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("peer fixture cleanup also failed: {cleanup:#}")))
        }
        (Ok(()), result) | (result, Ok(())) => result,
    }
}

pub(super) async fn stop_nodes(nodes: Vec<(Arc<Runtime>, PathBuf)>) -> Result<()> {
    for (node, _) in &nodes {
        node.shutdown().await;
    }
    let mut cleanup_errors = Vec::new();
    for (node, dir) in nodes {
        let weak = Arc::downgrade(&node);
        drop(node);
        if weak.upgrade().is_some() {
            cleanup_errors.push(anyhow::anyhow!(
                "shutdown left a strong Runtime owner for {}",
                dir.display()
            ));
        } else if let Err(error) = std::fs::remove_dir_all(&dir) {
            cleanup_errors.push(anyhow::anyhow!(
                "remove peer Node fixture directory {}: {error:#}",
                dir.display()
            ));
        }
    }
    if !cleanup_errors.is_empty() {
        let details = cleanup_errors
            .iter()
            .map(|error| format!("{error:#}"))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("peer fixture cleanup failed: {details}");
    }
    Ok(())
}

pub(super) fn remove_directory(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn first_sync(node: &Runtime, peer: &str) -> Result<()> {
    node.wake_sync(peer);
    tokio::time::timeout(Duration::from_secs(30), async {
        while !node
            .sync_peers
            .lock()
            .expect("sync peers")
            .get(peer)
            .is_some_and(|state| state.synced_ms.is_some() && state.failures == 0)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("first sync")
}

pub(super) fn posted(
    identity: &LocalIdentity,
    network: &str,
    scope: &str,
    id: &str,
    sequence: u64,
) -> Result<EventBody> {
    signed_main_input(
        &identity.fixture_admin(),
        network,
        scope,
        id,
        id,
        vec![],
        sequence,
    )
}

pub(super) fn pagination_reply(id: String) -> EventBody {
    EventBody::MessagePosted {
        message_id: id.clone(),
        role: Role::Assistant,
        text: id,
        in_reply_to: None,
        status: Some(ReplyStatus::Completed),
        failure: None,
        attachments: vec![],
        report_results: None,
        answers: None,
        attestation: None,
    }
}

pub(super) fn create_input_bot(
    store: &mut Store,
    author: &LocalIdentity,
    admin: &LocalIdentity,
    network: &str,
    scope: &str,
    at: i64,
) -> Result<SignedEvent> {
    let body = admin.attest(
        network,
        scope,
        EventBody::BotCreated {
            bot_id: scope.into(),
            name: format!("Bot {scope}"),
            instructions: String::new(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    store.append_event(author, network, scope, body, vec![], at)
}
