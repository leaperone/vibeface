use super::*;
use crate::connection::tests::{receive, send};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use vibeface_secure::{Channel, Handshake, membership::Genesis};

pub(super) async fn accept(
    listener: &tokio::net::TcpListener,
    node: &Identity,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    Channel,
)> {
    let (tcp, _) = listener.accept().await?;
    let mut socket = tokio_tungstenite::accept_async(tcp).await?;
    ensure!(
        matches!(socket.next().await.context("connect")??, Message::Text(_)),
        "connect missing"
    );
    let mut handshake = Handshake::signed(false, node, None, PROLOGUE)?;
    let Message::Binary(first) = socket.next().await.context("first handshake")?? else {
        bail!("expected Noise");
    };
    handshake.read(&first)?;
    socket
        .send(Message::Binary(handshake.write(&[])?.into()))
        .await?;
    let Message::Binary(last) = socket.next().await.context("last handshake")?? else {
        bail!("expected Noise");
    };
    handshake.read(&last)?;
    Ok((socket, handshake.finish()?))
}

#[tokio::test]
async fn rotation_preserves_members_and_saved_replacement_after_lost_acknowledgement() -> Result<()>
{
    let dir = std::env::temp_dir().join(format!("native-rotation-{}", uuid::Uuid::new_v4()));
    let mut key = ClientKey::create(&dir, "client password")?;
    let root = Identity::generate()?;
    let manager = Identity::generate()?;
    let node = Identity::generate()?;
    let mut state = NetworkState::new(Genesis::create(
        &root,
        Member::client(&manager, "manager", true)?,
        Member::node(&node, "node")?,
    )?)?;
    state.events.push(
        GovernanceEvent::unsigned(
            &state.project()?,
            &manager.device_id(),
            GovernanceAction::Grant {
                member: Member::client(&key.identity, "ordinary member", false)?,
            },
        )?
        .sign(&manager)?,
    );
    let before = state.project()?.members;
    let client_id = key.device_id().to_owned();
    let old_file = dir.join("old-recovery.json");
    let material = RecoveryMaterial {
        version: 1,
        network_id: state.genesis.network_id.clone(),
        root_secret_key: Zeroizing::new(root.secret_hex()),
        genesis: state.genesis.clone(),
        checkpoints: state.checkpoints.clone(),
        events: state.events.clone(),
    };
    durable::publish_new(&old_file, &recovery_file_bytes(&material, "old password")?)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let entry = Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: node.device_id(),
    };
    key.remember(&state.genesis.network_id, entry.clone())?;
    let task = tokio::spawn(async move {
        let (mut socket, mut channel) = accept(&listener, &node).await?;
        let request = receive(&mut socket, &mut channel).await?;
        assert_eq!(request["method"], "directory");
        send(
            &mut socket,
            &mut channel,
            json!({"id":request["id"],"result":{"state":state,"now_ms":now_ms()}}),
        )
        .await?;
        socket.close(None).await?;
        let (mut socket, mut channel) = accept(&listener, &node).await?;
        let request = receive(&mut socket, &mut channel).await?;
        assert_eq!(request["method"], "recover");
        let received: NetworkState = serde_json::from_value(request["params"].clone())?;
        let checkpoint = received.checkpoints.last().context("rotation checkpoint")?;
        assert!(checkpoint.directory.is_none());
        let view = received.project()?;
        assert_eq!(view.members, before);
        assert!(!view.client(&client_id)?.manage);
        let new_root = view.recovery_key.clone();
        socket.close(None).await?;
        let (mut socket, mut channel) = accept(&listener, &node).await?;
        let request = receive(&mut socket, &mut channel).await?;
        assert_eq!(
            request["method"], "authenticate",
            "lost acknowledgement must reconcile, never resubmit recover"
        );
        send(&mut socket, &mut channel, json!({"id":request["id"],"result":{"network_id":received.genesis.network_id,"state":received,"now_ms":now_ms()}})).await?;
        Ok::<_, anyhow::Error>(new_root)
    });
    let replacement = dir.join("new-recovery.json");
    let mut plan = prepare_key_rotation(
        &key,
        entry.clone(),
        &old_file,
        "old password",
        &replacement,
        "new password",
    )
    .await?;
    assert_eq!(plan.state(), IdentityOperationState::Prepared);
    assert!(plan.recovery_members().is_none());
    assert!(
        plan.validate_recovery_intent(&entry, &replacement, "ordinary member", None)
            .is_err(),
        "a pending key rotation cannot be retried as a member recovery"
    );
    let backup = read_recovery_file(&replacement, "new password")?;
    assert!(read_recovery_file(&replacement, "old password").is_err());
    let saved = std::fs::read(&replacement)?;
    plan.submit(&mut key).await?;
    assert_eq!(plan.state(), IdentityOperationState::Applied);
    assert_eq!(
        Identity::from_secret_hex(&backup.root_secret_key)?.device_id(),
        task.await??
    );
    assert_eq!(std::fs::read(&replacement)?, saved);
    drop(plan);
    let reopened = IdentityPlan::load(&key, "new password")?;
    assert_eq!(reopened.state(), IdentityOperationState::Applied);
    reopened.discard()?;
    assert!(replacement.exists());
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
