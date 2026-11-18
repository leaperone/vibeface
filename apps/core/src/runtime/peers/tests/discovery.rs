use super::super::test_support::{first_sync, node, remove_directory, stop_nodes};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::manager_settings;

fn copy_snapshot(source: &Path, destination: &Path) -> Result<()> {
    vibeface_bridge::platform::privatefs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "node.lock" {
            continue;
        }
        let from = entry.path();
        let to = destination.join(name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_snapshot(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to)?;
            vibeface_bridge::platform::privatefs::restrict_file(&to)?;
        } else {
            anyhow::bail!("unexpected entry in Node data snapshot: {}", from.display());
        }
    }
    Ok(())
}

/// A bot computer that goes offline is announced to clients and keeps its last capabilities,
/// so a client tells "its device is offline" from "no device runs a bot".
#[tokio::test(flavor = "multi_thread")]
async fn an_offline_node_is_announced_and_listed_with_its_last_capabilities() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        b.settings.lock().expect("settings").manager = Some(manager_settings(
            crate::providers::Provider::Cursor,
            dir_b.join("manager-workspace"),
            String::new(),
        ));
        a.net()?.learn(&b.net()?.addr()).await;
        let peer = ib.device_id()?;
        let access = crate::runtime::Access::User {
            network_id: network.clone(),
            scope: crate::runtime::UserScope::Network,
        };
        let listed = |nodes: Value| {
            nodes["nodes"]
                .as_array()
                .and_then(|nodes| nodes.iter().find(|node| node["device_id"] == *peer))
                .cloned()
                .context("the peer is listed")
        };
        first_sync(&a, &peer).await?;
        let online = listed(
            a.rpc(
                access.clone(),
                crate::command::business_fixture(json!({"method":"nodes"})),
            )
            .await?,
        )?;
        assert_eq!(online["online"], true);
        assert_eq!(online["capabilities"]["manager"], "cursor");
        let mut events = a.events.subscribe();
        b.stop_network().await;
        a.wake_sync(&peer);
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let update = events.recv().await?;
                if update.network_id == network && update.payload == json!({"type":"changed"}) {
                    return anyhow::Ok(());
                }
            }
        })
        .await
        .context("clients are told to reread once the peer stops answering")??;

        let offline = listed(
            a.rpc(
                access,
                crate::command::business_fixture(json!({"method":"nodes"})),
            )
            .await?,
        )?;
        assert_eq!(offline["online"], false);
        assert_eq!(offline["capabilities"], online["capabilities"]);
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}

/// The last capabilities of a member Node survive this Node's restart, so a bot computer that
/// is off still shows as offline instead of as missing.
#[tokio::test(flavor = "multi_thread")]
async fn an_offline_node_keeps_its_last_capabilities_across_a_restart() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let peer = ib.device_id()?;
    let access = crate::runtime::Access::User {
        network_id: ia.fixture_network_id()?,
        scope: crate::runtime::UserScope::Network,
    };
    let listed = |nodes: Value| {
        nodes["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|node| node["device_id"] == *peer))
            .cloned()
            .context("the peer is listed")
    };
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let online = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        b.settings.lock().expect("settings").manager = Some(manager_settings(
            crate::providers::Provider::Cursor,
            dir_b.join("manager-workspace"),
            String::new(),
        ));
        a.net()?.learn(&b.net()?.addr()).await;
        first_sync(&a, &peer).await?;
        listed(
            a.rpc(
                access.clone(),
                crate::command::business_fixture(json!({"method":"nodes"})),
            )
            .await?,
        )
    }
    .await;
    let identity = std::fs::read(dir_a.join("identity.json"))?;
    a.shutdown().await;
    b.shutdown().await;
    drop(a);
    drop(b);
    // Copy the closed data directory as a fresh process would receive it. The OS lock file
    // belongs to the old process and is intentionally not part of the snapshot.
    let copy = dir_a.with_extension("restarted");
    let result = async {
        let online = online?;
        assert_eq!(online["capabilities"]["manager"], "cursor");
        copy_snapshot(&dir_a, &copy).context("copy the data directory")?;
        assert_eq!(std::fs::read(copy.join("identity.json"))?, identity);
        let again = Arc::new(Runtime::open(&copy).await?);
        again.start_network().await?;
        let nodes = again
            .rpc(
                access.clone(),
                crate::command::business_fixture(json!({"method":"nodes"})),
            )
            .await;
        again.shutdown().await;
        drop(again);
        let offline = listed(nodes?)?;
        assert_eq!(offline["online"], false);
        assert_eq!(offline["capabilities"], online["capabilities"]);
        anyhow::Ok(())
    }
    .await;
    remove_directory(&copy)?;
    std::fs::remove_dir_all(dir_a)?;
    std::fs::remove_dir_all(dir_b)?;
    result
}
