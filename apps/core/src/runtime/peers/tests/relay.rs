use super::super::test_support::{finish_nodes, node, node_with};
use super::super::*;
use crate::crypto::LocalIdentity;

#[tokio::test]
async fn member_relay_clients_also_need_a_front_binding() -> Result<()> {
    use iroh_relay::server::AccessControl as _;
    let identity = LocalIdentity::generate();
    let (node, dir) = node(&identity).await?;
    let result = async {
        node.install_membership(identity.sign_network_fixture(1, vec![identity.device_id()?])?)
            .await?;
        let access = RelayAccess::new(Arc::downgrade(&node));
        for header in [None, Some("0"), Some("unknown-binding")] {
            let mut request = axum::http::Request::builder();
            if let Some(header) = header {
                request = request.header(net::relay::FRONT_HEADER, header);
            }
            let request = iroh_relay::server::ClientRequest::new(
                net::endpoint_id(&identity.device_id()?)?,
                iroh_relay::http::ProtocolVersion::V1,
                request.body(())?.into_parts().0,
            );
            assert_eq!(node.relay_admits(&request).await, Some(false));
            assert!(matches!(
                access.on_connect(&request).await,
                iroh_relay::server::Access::Deny { .. }
            ));
        }
        assert!(access.counted.lock().unwrap().is_empty());
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(node, dir)]).await
}

#[tokio::test]
async fn only_the_relay_a_member_runs_joins_the_relay_map() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (a, dir) = node(&ia).await?;
    let result = async {
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        a.install_membership(state).await?;
        a.start_network().await?;
        let net = a.net()?;
        let peer = net::endpoint_id(&ib.device_id()?)?;
        let home = net::relay_url("https://home.example.invalid")?;
        let runs = net::relay_url("https://runs.example.invalid")?;
        let endpoint =
            iroh::EndpointAddr::from_parts(peer, [iroh::TransportAddr::Relay(home.clone())]);
        net.learn(&endpoint).await;
        ensure!(
            !net.knows_relay(&home).await,
            "a peer's home relay stays out"
        );
        net.learn_member(&AddressRecord {
            version: 1,
            network_id: ia.fixture_network_id()?,
            device_id: ib.device_id()?,
            seq: 1,
            endpoint,
            gateway_url: None,
            relay: Some(runs.to_string()),
        })
        .await;
        ensure!(
            !net.knows_relay(&home).await,
            "the member's home relay stays out"
        );
        ensure!(
            net.knows_relay(&runs).await,
            "the relay the member runs joins"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish_nodes(result, vec![(a, dir)]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_with_disjoint_ip_families_sync_through_a_member_relay() -> Result<()> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let relay_url = format!("http://127.0.0.1:{port}");
    let ids = [
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    ];
    let state = ids[0].sign_network_fixture(
        1,
        ids.iter()
            .map(LocalIdentity::device_id)
            .collect::<Result<_>>()?,
    )?;
    let network = ids[0].fixture_network_id()?;
    let (relay, dir_r) = node_with(
        &ids[0],
        net::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            relay_listen: Some(format!("127.0.0.1:{port}").parse()?),
            ..Default::default()
        },
    )
    .await?;
    let (a, dir_a) = node_with(
        &ids[1],
        net::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            relays: vec![net::Relay {
                url: relay_url.clone(),
                token: None,
            }],
            ..Default::default()
        },
    )
    .await?;
    let (b, dir_b) = node_with(
        &ids[2],
        net::NetworkConfig {
            bind: Some("[::1]:0".parse()?),
            relays: vec![net::Relay {
                url: relay_url.clone(),
                token: None,
            }],
            ..Default::default()
        },
    )
    .await?;
    let result = async {
        for node in [&relay, &a, &b] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        {
            let arg0 = (ids[1]).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("c").to_owned();
            let arg3 = ("Relayed").to_owned();
            let arg4 = ids[0].owner_fixture(&network, "c", &ids[1].device_id()?)?;
            a.store
                .call("peers.create_conversation", move |store| {
                    store.create_conversation(&arg0, &arg1, &arg2, &arg3, arg4, now_ms())
                })
                .await
        }?;
        let addr = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let addr = a.net()?.addr();
                if addr.relay_urls().next().is_some() {
                    return Ok::<_, anyhow::Error>(addr);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("the hidden Node never reached its relay")??;
        assert!(addr.ip_addrs().all(|address| address.is_ipv4()));
        assert!(b.net()?.addr().ip_addrs().all(|address| address.is_ipv6()));
        b.net()?.learn(&addr).await;
        b.sync_peer(&network, &ids[1].device_id()?).await?;
        assert_eq!(
            {
                let arg0 = (network).to_owned();
                b.store
                    .call("peers.heads", move |store| store.heads(&arg0))
                    .await
            }?,
            {
                let arg0 = (network).to_owned();
                a.store
                    .call("peers.heads", move |store| store.heads(&arg0))
                    .await
            }?
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for node in [&relay, &a, &b] {
        node.stop_network().await;
    }
    drop((relay, a, b));
    for dir in [dir_r, dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}
