use super::super::test_support::{create_input_bot, node};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::signed_main_input;

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_fetches_a_missing_attachment_from_its_author_or_marks_it_pending() -> Result<()> {
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
        let content = b"meeting notes".to_vec();
        let blob = BlobRef {
            hash: crate::crypto::digest(&content),
            size: content.len() as u64,
        };
        let attached = Attachment {
            hash: blob.hash.clone(),
            size: blob.size,
            name: "notes.txt".into(),
        };
        {
            let ia = ia.clone();
            let network = network.clone();
            let blob = blob.clone();
            let content = content.clone();
            let attached = attached.clone();
            a.store
                .call("peers.fixture", move |store| {
                    let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                    store.create_conversation(&ia, &network, "c", "Files", owner, now_ms())?;
                    create_input_bot(store, &ia, &ia.fixture_admin(), &network, "c", now_ms())?;
                    store.put_blob(&blob, &content)?;
                    store.post_input_fixture(
                        &ia,
                        &network,
                        signed_main_input(
                            &ia.fixture_admin(),
                            &network,
                            "c",
                            "m1",
                            "m1",
                            vec![attached],
                            1,
                        )?,
                        now_ms(),
                    )?;

                    Ok(())
                })
                .await?
        }
        b.net()?.learn(&a.net()?.addr()).await;
        b.refresh_own_address(&network).await?;
        b.sync_peer(&network, &ia.device_id()?).await?;
        let events = {
            let network = network.clone();
            b.store
                .call("peers.attachment_context", move |store| {
                    Ok(crate::store::context_after(
                        &store.connection,
                        &network,
                        "c",
                        None,
                        &VersionVector::new(),
                    )?
                    .events)
                })
                .await?
        };
        let file = {
            let arg0 = (blob.hash).to_owned();
            b.store
                .call("peers.blob_file", move |store| store.blob_file(&arg0))
                .await
        }?;
        std::fs::remove_file(&file)?;
        assert!(
            b.fetch_turn_attachments(&network, events.iter())
                .await
                .is_empty()
        );
        assert!(
            {
                let arg0 = (blob).to_owned();
                b.store
                    .call("peers.has_blob", move |store| store.has_blob(&arg0))
                    .await
            }?,
            "fetched from the message's author"
        );
        a.stop_network().await;
        std::fs::remove_file(&file)?;
        assert_eq!(
            b.fetch_turn_attachments(&network, events.iter()).await,
            [blob.hash.clone()].into(),
            "an author out of reach leaves it pending"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for node in [&a, &b] {
        node.stop_network().await;
    }
    drop((a, b));
    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_fetches_attachments_past_one_the_peer_lacks_and_catches_up_later() -> Result<()> {
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
        let mut files: Vec<(Vec<u8>, BlobRef)> = ["first notes", "second notes"]
            .map(|text| {
                let content = text.as_bytes().to_vec();
                let blob = BlobRef {
                    hash: crate::crypto::digest(&content),
                    size: content.len() as u64,
                };
                (content, blob)
            })
            .into();
        files.sort_by(|x, y| x.1.hash.cmp(&y.1.hash));
        let attached: Vec<Attachment> = files
            .iter()
            .map(|(_, blob)| Attachment {
                hash: blob.hash.clone(),
                size: blob.size,
                name: "notes.txt".into(),
            })
            .collect();
        let (lacked, held) = (&files[0], &files[1]);
        {
            let ia = ia.clone();
            let network = network.clone();
            let files = files.clone();
            let attached = attached.clone();
            let lacked = (*lacked).clone();
            a.store
                .call("peers.fixture", move |store| {
                    let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                    store.create_conversation(&ia, &network, "c", "Files", owner, now_ms())?;
                    create_input_bot(store, &ia, &ia.fixture_admin(), &network, "c", now_ms())?;
                    for (content, blob) in &files {
                        store.put_blob(blob, content)?;
                    }
                    store.post_input_fixture(
                        &ia,
                        &network,
                        signed_main_input(
                            &ia.fixture_admin(),
                            &network,
                            "c",
                            "m1",
                            "m1",
                            attached,
                            1,
                        )?,
                        now_ms(),
                    )?;
                    std::fs::remove_file(store.blob_file(&lacked.1.hash)?)?;

                    Ok(())
                })
                .await?
        }
        b.net()?.learn(&a.net()?.addr()).await;
        b.refresh_own_address(&network).await?;
        b.sync_peer(&network, &ia.device_id()?).await?;
        assert!(
            {
                let arg0 = (held.1).to_owned();
                b.store
                    .call("peers.has_blob", move |store| store.has_blob(&arg0))
                    .await
            }?,
            "an attachment the peer lacks does not hold back the next"
        );
        assert_eq!(
            {
                let arg0 = (network).to_owned();
                b.store
                    .call("peers.missing_blobs", move |store| {
                        store.missing_blobs(&arg0)
                    })
                    .await
            }?,
            std::slice::from_ref(&lacked.1)
        );
        let partial = {
            let hash = lacked.1.hash.clone();
            b.store
                .call("peers.partial_blob_file", move |store| {
                    store.partial_blob_file(&hash)
                })
                .await?
        };
        assert!(
            !partial.exists(),
            "an attachment the peer lacks stages nothing"
        );
        {
            let arg0 = (lacked.1).to_owned();
            let arg1 = (lacked.0).to_owned();
            a.store
                .call("peers.put_blob", move |store| store.put_blob(&arg0, &arg1))
                .await
        }?;
        b.sync_peer(&network, &ia.device_id()?).await?;
        assert!(
            {
                let arg0 = (network).to_owned();
                b.store
                    .call("peers.missing_blobs", move |store| {
                        store.missing_blobs(&arg0)
                    })
                    .await
            }?
            .is_empty(),
            "the next sync round fetches what was pending"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for node in [&a, &b] {
        node.stop_network().await;
    }
    drop((a, b));
    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}
