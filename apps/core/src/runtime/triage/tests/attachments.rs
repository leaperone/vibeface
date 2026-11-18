use super::super::test_support::with_node;
use super::super::*;
use crate::runtime::test_support::{HASH_PAUSES, bot};

#[tokio::test(flavor = "multi_thread")]
async fn uploads_serialize_with_downloads_and_hash_without_the_store_lock() -> Result<()> {
    use base64::Engine as _;
    with_node(async |node, admin, _| {
        bot(&node, &admin, "c", "Attachments", &[]).await?;
        let network = node.network_id().await?;
        let access = crate::runtime::Access::User {
            network_id: network.clone(),
            scope: crate::runtime::UserScope::Network,
        };
        let content = vec![b'a'; 1024 * 1024 + 16];
        let hash = crate::crypto::digest(&content);
        let chunk = |offset: usize, bytes: &[u8]| {
            json!({"conversation_id":"c","hash":hash,"size":content.len(),"offset":offset,
            "data":base64::engine::general_purpose::STANDARD.encode(bytes)})
        };
        let first = chunk(0, &content[..1024 * 1024]);
        let (one, duplicate) = tokio::join!(
            node.upload_attachment(&access, &first),
            node.upload_attachment(&access, &first)
        );
        assert_eq!(one?, json!({"offset":1024 * 1024,"complete":false}));
        assert_eq!(duplicate?, json!({"offset":1024 * 1024,"complete":false}));
        let (part, target) = {
            let hash = hash.clone();
            node.store
                .call("triage.test.partial_blob_paths", move |store| {
                    assert_eq!(store.committed_offset(&hash)?, 1024 * 1024);
                    Ok((store.partial_blob_file(&hash)?, store.blob_file(&hash)?))
                })
                .await?
        };
        {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&part)?
                .write_all(b"unsynced suffix")?;
        }
        let (entered, hashing) = tokio::sync::oneshot::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        lock(&HASH_PAUSES).insert(part.clone(), (entered, paused));
        let final_chunk = chunk(1024 * 1024, &content[1024 * 1024..]);
        let uploader = node.clone();
        let upload =
            tokio::spawn(async move { uploader.upload_attachment(&access, &final_chunk).await });
        let entered = tokio::time::timeout(Duration::from_secs(5), hashing).await;
        if !matches!(entered, Ok(Ok(()))) {
            let _ = resume.send(());
            lock(&HASH_PAUSES).remove(&part);
            upload.abort();
            bail!("the upload did not reach its hash barrier");
        }
        let store_available = tokio::time::timeout(Duration::from_millis(250), async {
            let hash = hash.clone();
            let network = network.clone();
            let boot = node.boot.clone();
            let content_len = content.len();
            node.store
                .call("triage.test.blob_store_available", move |store| {
                    ensure!(
                        store.committed_offset(&hash)? == content_len as u64,
                        "the hash barrier follows the durable offset"
                    );
                    store.triage_status(&network, &boot, None, 100, now_ms())
                })
                .await
        })
        .await;
        let downloader = node.clone();
        let downloaded_hash = hash.clone();
        let downloaded_network = network.clone();
        let mut download = tokio::spawn(async move {
            downloader
                .fetch_blob(
                    &downloaded_network,
                    "unavailable-peer",
                    &crate::model::BlobRef {
                        hash: downloaded_hash,
                        size: 1024 * 1024 + 16,
                    },
                )
                .await
        });
        let download_before_hash =
            tokio::time::timeout(Duration::from_millis(50), &mut download).await;
        let download_waited = download_before_hash.is_err();
        let _ = resume.send(());
        assert_eq!(
            upload.await??,
            json!({"offset":content.len(),"complete":true})
        );
        match download_before_hash {
            Ok(result) => result??,
            Err(_) => download.await??,
        }
        store_available.context("hashing must leave the Store available")??;
        assert!(
            download_waited,
            "the download must share the partial-file lock"
        );
        assert_eq!(std::fs::read(&target)?, content);
        assert!(!part.exists());
        assert_eq!(
            {
                let hash = (hash).to_owned();
                node.store
                    .call("triage.test.committed_offset", move |store| {
                        store.committed_offset(&hash)
                    })
                    .await
            }?,
            0
        );
        let bad_hash = crate::crypto::digest(b"good");
        let failed = node
            .upload_attachment(
                &crate::runtime::Access::User {
                    network_id: network,
                    scope: crate::runtime::UserScope::Network,
                },
                &json!({"conversation_id":"c","hash":bad_hash,"size":4,"offset":0,
                "data":base64::engine::general_purpose::STANDARD.encode(b"evil")}),
            )
            .await;
        assert!(failed.is_err());

        node.store
            .call("triage.test.bad_blob_absent", move |store| {
                assert_eq!(store.committed_offset(&bad_hash)?, 0);
                assert!(!store.partial_blob_file(&bad_hash)?.exists());
                assert!(!store.blob_file(&bad_hash)?.exists());
                Ok::<_, anyhow::Error>(())
            })
            .await
    })
    .await
}
