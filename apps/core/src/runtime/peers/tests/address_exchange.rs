use super::super::test_support::{finish_nodes, node, ready_member_rpc};
use super::super::*;
use crate::crypto::LocalIdentity;

fn address_fixture(
    identity: &LocalIdentity,
    network: &str,
    seq: u64,
    gateway: Option<&str>,
) -> Result<SignedAddress> {
    let device = identity.device_id()?;
    let payload = serde_json::to_string(&AddressRecord {
        version: 1,
        network_id: network.into(),
        endpoint: iroh::EndpointAddr::new(net::endpoint_id(&device)?),
        device_id: device,
        seq,
        gateway_url: gateway.map(String::from),
        relay: None,
    })?;
    Ok(SignedAddress {
        signature: identity.sign_payload(net::ADDRESS_DOMAIN, payload.as_bytes())?,
        payload,
    })
}

async fn address_book_fixture(node: &Runtime) -> Result<Vec<SignedAddress>> {
    node.store
        .call("address_fixture.book", |store| store.address_book())
        .await
}

async fn address_refusal_fixture(connection: &Connection, code: u32, reason: &[u8]) -> Result<()> {
    let closed = tokio::time::timeout(Duration::from_secs(3), connection.closed())
        .await
        .context("expected a prompt authority refusal, not a connection timeout")?;
    let ConnectionError::ApplicationClosed(close) = closed else {
        bail!("expected an application authority refusal, got {closed}");
    };
    ensure!(
        close.error_code == code.into(),
        "unexpected authority close code: {} (expected {code})",
        close.error_code
    );
    ensure!(
        close.reason.as_ref() == reason,
        "unexpected authority close reason: {:?} (expected {reason:?})",
        close.reason
    );
    Ok(())
}

// The same malformed wire bodies exercise both production decode sites. These are JSON
// mutations, not a second implementation of the private AddressExchange contract.
fn malformed_address_bodies(valid: &SignedAddress) -> Vec<Value> {
    let marker = "address-envelope-secret-marker";
    let mut over_cap = vec![json!(valid); MAX_ADDRESS_RECORDS];
    over_cap.push(json!({"payload": marker}));
    vec![
        json!({}),
        json!({"records":null}),
        json!({"records":marker}),
        json!({"records":23}),
        json!({"records":false}),
        json!({"records":{"payload":marker}}),
        json!({"records":[null]}),
        json!({"records":[{"payload":marker}]}),
        json!({"records":[{"signature":marker}]}),
        json!({"records":[{"payload":17,"signature":marker}]}),
        json!({"records":[{"payload":marker,"signature":false}]}),
        json!({"records":[{"payload":marker,"signature":marker,(marker):marker}]}),
        json!({"records":[valid,{"payload":marker}]}),
        json!({"records":over_cap}),
    ]
}

#[tokio::test]
async fn address_exchange_request_shape_filtering_and_authority_over_quic() -> Result<()> {
    use crate::identity::{GovernanceEvent, Revocation};
    let (ia, ib, ic, outside) = (
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    );
    let network = ia.fixture_network_id()?;
    let peer = ia.device_id()?;
    let mut state =
        ia.sign_network_fixture(1, vec![peer.clone(), ib.device_id()?, ic.device_id()?])?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = match node(&ib).await {
        Ok(node) => node,
        Err(error) => return finish_nodes(Err(error), vec![(a, dir_a)]).await,
    };
    let (outsider, dir_out) = match node(&outside).await {
        Ok(node) => node,
        Err(error) => return finish_nodes(Err(error), vec![(a, dir_a), (b, dir_b)]).await,
    };
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
            // Suppress automatic gossip so only these explicit wire requests can write
            // third-member records. The actual production router/admission still runs.
            for member in &state.project()?.members {
                node.sync_peers
                    .lock()
                    .map_err(|_| anyhow::anyhow!("fixture sync peer lock poisoned"))?
                    .entry(member.device_id.clone())
                    .or_default()
                    .running = true;
            }
            node.start_network().await?;
            node.refresh_own_address(&network).await?;
        }
        outsider.start_network().await?;
        for sender in [&b, &outsider] {
            sender.net()?.learn(&a.net()?.addr()).await;
        }
        let seq = now_ms() as u64 + 10_000;
        let valid = address_fixture(&ic, &network, seq, Some("https://entry.example"))?;
        let before = address_book_fixture(&a).await?;
        for mut body in malformed_address_bodies(&valid) {
            body["type"] = json!("addresses");
            body["network_id"] = json!(network);
            let error = b
                .net()?
                .request(&peer, net::RPC, &body, PEER_TIMEOUT)
                .await
                .err()
                .context("malformed request must fail")?;
            ensure!(
                format!("{error:#}") == "invalid address exchange records",
                "malformed request must return the fixed protocol error"
            );
            ensure!(
                !format!("{error:#}").contains("address-envelope-secret-marker"),
                "malformed request error must not echo the marker"
            );
            ensure!(
                address_book_fixture(&a).await? == before,
                "malformed request must not write even a valid prefix"
            );
        }
        let mut pull = AddressExchange { records: vec![] }.into_request(&network)?;
        pull["existing_routing_extra"] = json!(true);
        let reply = b
            .net()?
            .request(&peer, net::RPC, &pull, PEER_TIMEOUT)
            .await?;
        ensure!(
            AddressExchange::decode(&reply)?.records == before,
            "empty pull with routing extras returns the existing address book"
        );

        let mut bad_signature = valid.clone();
        bad_signature.signature = "address-envelope-secret-marker".into();
        let invalid_payload = SignedAddress {
            payload: "address-envelope-secret-marker".into(),
            signature: valid.signature.clone(),
        };
        let records = vec![
            bad_signature,
            invalid_payload,
            address_fixture(&ic, &outside.fixture_network_id()?, seq + 10, None)?,
            address_fixture(&outside, &network, seq + 10, None)?,
            address_fixture(&ia, &network, seq + 10, None)?,
            valid.clone(),
        ];
        b.net()?
            .request(
                &peer,
                net::RPC,
                &AddressExchange { records }.into_request(&network)?,
                PEER_TIMEOUT,
            )
            .await?;
        let expected = before
            .into_iter()
            .chain([valid.clone()])
            .map(|signed| Ok((signed.open(&network)?.device_id, signed)))
            .collect::<Result<BTreeMap<_, _>>>()?
            .into_values()
            .collect::<Vec<_>>();
        ensure!(
            address_book_fixture(&a).await? == expected,
            "only the valid current-member address may be added"
        );
        let withdrawn = address_fixture(&ic, &network, seq + 1, None)?;
        for record in [withdrawn.clone(), valid] {
            b.net()?
                .request(
                    &peer,
                    net::RPC,
                    &AddressExchange {
                        records: vec![record],
                    }
                    .into_request(&network)?,
                    PEER_TIMEOUT,
                )
                .await?;
        }
        let retained = a.entrances(&network).await?;
        ensure!(
            retained.contains(&withdrawn),
            "withdrawal survives an older advertisement"
        );
        let mut current = None;
        for signed in address_book_fixture(&a).await? {
            if signed.open(&network)?.device_id == ic.device_id()? {
                current = Some(signed);
            }
        }
        ensure!(
            current.as_ref() == Some(&withdrawn),
            "stored seq high water must retain the withdrawal"
        );

        // Direct Net calls deliberately bypass peer_request's local authorization checks.
        let before = address_book_fixture(&a).await?;
        let malformed = json!({"type":"addresses","network_id":network,
            "records":"address-envelope-secret-marker"});
        let mut wrong_network = malformed.clone();
        wrong_network["network_id"] = json!(outside.fixture_network_id()?);
        ready_member_rpc(&b, &network, &peer).await?;
        let wrong_connection = b.net()?.connect(&peer, net::RPC).await?;
        ensure!(
            wrong_connection.close_reason().is_none(),
            "wrong-network probe starts on a healthy admitted connection"
        );
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            b.net()?
                .request(&peer, net::RPC, &wrong_network, PEER_TIMEOUT),
        )
        .await
        .context("wrong network must be refused promptly")?
        .err()
        .context("wrong network must fail")?;
        address_refusal_fixture(&wrong_connection, 2, b"wrong personal network").await?;
        ensure!(
            !format!("{error:#}").contains("invalid address exchange records"),
            "network refusal precedes records parsing"
        );
        ensure!(
            !format!("{error:#}").contains("address-envelope-secret-marker"),
            "network refusal must not echo the marker"
        );
        ensure!(
            address_book_fixture(&a).await? == before,
            "wrong-network refusal must not write addresses"
        );
        let outside_connection = outsider.net()?.connect(&peer, net::RPC).await?;
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            net::call(&outside_connection, &malformed),
        )
        .await
        .context("outsider must be refused promptly")?
        .err()
        .context("outsider must fail")?;
        address_refusal_fixture(
            &outside_connection,
            1,
            b"not a member Node of this personal network",
        )
        .await?;
        ensure!(
            !format!("{error:#}").contains("invalid address exchange records"),
            "outsider refusal precedes records parsing"
        );
        ensure!(
            !format!("{error:#}").contains("address-envelope-secret-marker"),
            "outsider refusal must not echo the marker"
        );
        ensure!(
            address_book_fixture(&a).await? == before,
            "outsider refusal must not write addresses"
        );
        ready_member_rpc(&b, &network, &peer).await?;
        let cached = b.net()?.connect(&peer, net::RPC).await?;
        ensure!(
            cached.close_reason().is_none(),
            "revocation probe retains the healthy admitted connection"
        );
        state.append(GovernanceEvent::revoke(
            &ia.fixture_admin(),
            &state.project()?,
            Revocation::new(&ib.device_id()?, None)?,
        )?)?;
        a.install_membership(state).await?;
        let error = tokio::time::timeout(Duration::from_secs(3), net::call(&cached, &malformed))
            .await
            .context("revoked cached request must be refused promptly")?
            .err()
            .context("revoked cached request must fail")?;
        address_refusal_fixture(&cached, 1, b"no longer a member Node").await?;
        ensure!(
            !format!("{error:#}").contains("invalid address exchange records"),
            "revoked connection refusal precedes records parsing"
        );
        ensure!(
            !format!("{error:#}").contains("address-envelope-secret-marker"),
            "revoked connection refusal must not echo the marker"
        );
        ensure!(
            address_book_fixture(&a).await? == before,
            "revoked connection refusal must not write addresses"
        );
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir_a), (b, dir_b), (outsider, dir_out)]).await
}

#[tokio::test]
async fn address_exchange_reply_shape_and_history_continuation_over_quic() -> Result<()> {
    #[derive(Clone, Debug)]
    struct ReplyFixture(Arc<std::sync::Mutex<Value>>);
    impl ProtocolHandler for ReplyFixture {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            let reply = self.0.clone();
            net::serve_streams(connection, move |request: Value| {
                let reply = reply.clone();
                async move {
                    ensure!(
                        request["type"] == "addresses",
                        "fixture only answers addresses"
                    );
                    Ok(reply
                        .lock()
                        .map_err(|_| anyhow::anyhow!("fixture reply lock poisoned"))?
                        .clone())
                }
            })
            .await;
            Ok(())
        }
    }
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let peer = ib.device_id()?;
    let network = ia.fixture_network_id()?;
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, peer.clone()])?;
    let response = Arc::new(std::sync::Mutex::new(json!({"records":[]})));
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = match node(&ib).await {
        Ok(node) => node,
        Err(error) => return finish_nodes(Err(error), vec![(a, dir_a)]).await,
    };
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
        }
        a.sync_peers
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture sync peer lock poisoned"))?
            .entry(peer.clone())
            .or_default()
            .running = true;
        a.start_network().await?;
        a.refresh_own_address(&network).await?;
        // A controlled reply on RPC; SYNC retains the production Inbound handler and
        // accept_connection authority checks against b's current Node membership.
        let net = Arc::new(Net::bind(&ib.secret_key_hex, &b.config.network, 0, vec![]).await?);
        if b.net.set(net.clone()).is_err() {
            net.close().await;
            bail!("fixture network starts once");
        }
        *b.router.lock().await = Some(
            Router::builder(net.endpoint.clone())
                .accept(net::RPC, ReplyFixture(response.clone()))
                .accept(
                    net::SYNC,
                    Inbound {
                        node: Arc::downgrade(&b),
                        alpn: net::SYNC,
                    },
                )
                .spawn(),
        );
        a.net()?.learn(&net.addr()).await;
        b.refresh_own_address(&network).await?;
        let valid = address_book_fixture(&b)
            .await?
            .into_iter()
            .next()
            .context("reply fixture has its production signed endpoint")?;
        let before = address_book_fixture(&a).await?;
        for body in malformed_address_bodies(&valid) {
            *response
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture reply lock poisoned"))? = body;
            let error = a
                .exchange_addresses(&network, &peer)
                .await
                .err()
                .context("malformed reply must fail")?;
            ensure!(
                format!("{error:#}") == "invalid address exchange records",
                "malformed reply must return the fixed protocol error"
            );
            ensure!(
                !format!("{error:#}").contains("address-envelope-secret-marker"),
                "malformed reply error must not echo the marker"
            );
            ensure!(
                address_book_fixture(&a).await? == before,
                "malformed reply must not write even a valid prefix"
            );
        }
        *response
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture reply lock poisoned"))? =
            serde_json::to_value(AddressExchange { records: vec![] })?;
        a.exchange_addresses(&network, &peer).await?;
        ensure!(
            address_book_fixture(&a).await? == before,
            "empty reply must not change the address book"
        );
        let mut invalid = valid.clone();
        invalid.signature = "address-envelope-secret-marker".into();
        *response
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture reply lock poisoned"))? =
            serde_json::to_value(AddressExchange {
                records: vec![invalid, valid.clone()],
            })?;
        a.exchange_addresses(&network, &peer).await?;
        ensure!(
            address_book_fixture(&a).await?.contains(&valid),
            "valid reply record survives individual invalid signature filtering"
        );

        let owner = ia.owner_fixture(&network, "address-history", &peer)?;
        let (identity, network_for_store) = (ib.clone(), network.clone());
        b.store
            .call("address_fixture.history", move |store| {
                store.create_conversation(
                    &identity,
                    &network_for_store,
                    "address-history",
                    "History after malformed address reply",
                    owner,
                    now_ms(),
                )?;
                Ok(())
            })
            .await?;
        let seq = b.local_seq(&network, &peer).await?;
        ensure!(
            seq > a.local_seq(&network, &peer).await?,
            "history continuation begins with missing signed history"
        );
        *response
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture reply lock poisoned"))? =
            json!({"records":"address-envelope-secret-marker"});
        ensure!(
            a.exchange_addresses(&network, &peer).await.is_err(),
            "address exchange must fail before the history continuation check"
        );
        a.sync_peer(&network, &peer).await?;
        ensure!(
            a.local_seq(&network, &peer).await? == seq,
            "a malformed address reply still allows the signed history pull"
        );
        ensure!(
            address_book_fixture(&a).await?.len() == before.len() + 1,
            "history sync with a malformed address reply must not add addresses"
        );
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir_a), (b, dir_b)]).await
}
