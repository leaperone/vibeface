use super::super::test_support::{create_input_bot, finish_nodes, node, posted, ready_member_rpc};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::signed_main_input;

#[tokio::test(flavor = "multi_thread")]
async fn member_nodes_sync_ranges_and_attachments_without_jobs_and_refuse_outsiders() -> Result<()>
{
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let (outsider, dir_c) = node(&LocalIdentity::generate()).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
        }
        for node in [&a, &b, &outsider] {
            node.start_network().await?;
        }
        let content: Vec<u8> = (0..3 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        let blob = BlobRef {
            hash: crate::crypto::digest(&content),
            size: content.len() as u64,
        };
        let attached = Attachment { hash: blob.hash.clone(), size: blob.size, name: "notes.bin".into() };
        {
            let ia = ia.clone();
            let network = network.clone();
            let blob = blob.clone();
            let content = content.clone();
            let attached = attached.clone();
            a.store.call("peers.fixture", move |store| {
            let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
            store.create_conversation(&ia, &network, "c", "Sync", owner, now_ms())?;
            create_input_bot(store, &ia, &ia.fixture_admin(), &network, "c", now_ms())?;
            store.put_blob(&blob, &content)?;
            store.post_input_fixture(&ia, &network, signed_main_input(&ia.fixture_admin(), &network, "c", "m1", "m1", vec![attached.clone()], 1)?, now_ms())?;

                Ok(())
            }).await?
        }
        b.net()?.learn(&a.net()?.addr()).await;
        b.refresh_own_address(&network).await?;
        let peer_a = ia.device_id()?;
        let first = {
            let (b, network, peer) = (b.clone(), network.clone(), peer_a.clone());
            tokio::spawn(async move { b.sync_peer(&network, &peer).await })
        };
        while {
            let arg0 = (blob.hash).to_owned();
            b.store.call("peers.committed_offset", move |store| store.committed_offset(&arg0)).await
        }? == 0
            && !first.is_finished()
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        b.fetch_blobs(&network, &peer_a).await?;
        first.await??;
        let heads = {
            let arg0 = (network).to_owned();
            a.store.call("peers.heads", move |store| store.heads(&arg0)).await
        }?;
        assert_eq!({
            let arg0 = (network).to_owned();
            b.store.call("peers.heads", move |store| store.heads(&arg0)).await
        }?, heads);
        assert!({
            let arg0 = (network).to_owned();
            b.store.call("peers.missing_blobs", move |store| store.missing_blobs(&arg0)).await
        }?.is_empty());
        let synced = {
            let arg0 = (network).to_owned();
            let arg1 = ("c").to_owned();
            let arg2 = ("m1").to_owned();
            b.store.call("peers.message", move |store| store.message(&arg0, &arg1, &arg2)).await
        }?.context("m1 synced")?;
        assert_eq!(synced.attachments, vec![attached], "the file name syncs with the message");
        assert!({
            let arg0 = ("c").to_owned();
            b.store.call("peers.scheduler_jobs", move |store| store.scheduler_jobs(&arg0)).await
        }?.is_empty());
        assert!(b.store.call("peers.scheduler_outbox", move |store| store.scheduler_outbox()).await?.is_empty());

        for id in ["m2", "m3"] {
            {
                let arg0 = (ia).to_owned();
                let arg1 = (network).to_owned();
                let arg2 = posted(&ia, &network, "c", id, if id == "m2" { 2 } else { 3 })?;
                a.store.call("peers.post_input_fixture", move |store| store.post_input_fixture(&arg0, &arg1, arg2, now_ms())).await
            }?;
        }
        let mut updates = b.events.subscribe();
        b.sync_peer(&network, &ia.device_id()?).await?;
        b.sync_peer(&network, &ia.device_id()?).await?;
        let mut pushed = Vec::new();
        while let Ok(update) = updates.try_recv() {
            let records = b.changed_records(&network, "browser", &update.payload).await?
                .context("an ingested message is pushed as records")?;
            assert_eq!((&records["type"], &records["conversation_id"]), (&json!("records"), &json!("c")));
            for message in records["messages"].as_array().context("messages")? {
                assert!(message["rank"].is_string(), "clients order pushed messages by rank");
                pushed.push(message["message_id"].clone());
            }
        }
        pushed.sort_by_key(|id| id.to_string());
        assert_eq!(pushed, [json!("m2"), json!("m3")], "each ingested message is pushed once");
        assert_eq!({
            let arg0 = (network).to_owned();
            b.store.call("peers.heads", move |store| store.heads(&arg0)).await
        }?, {
            let arg0 = (network).to_owned();
            a.store.call("peers.heads", move |store| store.heads(&arg0)).await
        }?);
        assert_eq!({
            let arg0 = (network).to_owned();
            let arg1 = ("c").to_owned();
            b.store.call("peers.messages", move |store| store.messages(&arg0, &arg1)).await
        }?.len(), 3);
        assert!({
            let arg0 = ("c").to_owned();
            b.store.call("peers.scheduler_jobs", move |store| store.scheduler_jobs(&arg0)).await
        }?.is_empty());
        assert!(
            a.store.call("peers.address_book", move |store| store.address_book()).await?.iter().any(|signed| signed
                .open(&network)
                .is_ok_and(|record| record.device_id == ib.device_id().unwrap_or_default())),
            "the pulled Node learned the puller's signed address"
        );
        a.sync_peer(&network, &ib.device_id()?).await?;

        ready_member_rpc(&b, &network, &peer_a).await?;
        {
            let arg0 = (ib).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = posted(&ia, &network, "c", "m4", 4)?;
            b.store.call("peers.post_input_fixture", move |store| store.post_input_fixture(&arg0, &arg1, arg2, now_ms())).await
        }?;
        assert_eq!(b.store.call("peers.scheduler_outbox", move |store| store.scheduler_outbox()).await?.len(), 1);
        b.deliver_outbox().await?;
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            while !b.store.call("peers.scheduler_outbox", move |store| store.scheduler_outbox()).await?.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            anyhow::Ok(())
        }).await;
        if drained.is_err() {
            let sender = {
                let network = network.clone();
                b.store.call("peers.mailbox_diagnostic", move |store| store.mailbox_for_test(&network, "c", "message:m4")).await.ok().flatten()
            };
            let owner = {
                let network = network.clone();
                a.store.call("peers.owner_diagnostic", move |store| Ok((
                    store.message(&network, "c", "m4").ok().map(|message| message.is_some()),
                    store.scheduler_job("c", "message:m4").ok().map(|job| job.is_some()),
                ))).await.ok()
            };
            bail!("owner returns the stored delivery receipt: sender_state={:?}, attempts={:?}, next_attempt_in_ms={:?}, owner_has_message_and_job={owner:?}",
                sender.as_ref().map(|record| record.state),
                sender.as_ref().map(|record| record.attempts),
                sender.as_ref().map(|record| record.next_attempt_ms.saturating_sub(now_ms()).max(0)));
        }
        drained??;
        let trigger = crate::scheduling::message_trigger("m4");
        assert_eq!(
            {
                let arg0 = (network).to_owned();
                let arg1 = ("c").to_owned();
                let arg2 = (trigger).to_owned();
                b.store.call("peers.mailbox_for_test", move |store| store.mailbox_for_test(&arg0, &arg1, &arg2)).await
            }?
                .map(|record| record.state),
            Some(crate::store::mailbox::MailboxState::Delivered)
        );
        {
            let arg0 = ("c").to_owned();
            let arg1 = (trigger).to_owned();
            a.store.call("peers.scheduler_job", move |store| store.scheduler_job(&arg0, &arg1)).await
        }?
            .context("the owner admitted the delivered message")?;
        let command = {
            let arg0 = (network).to_owned();
            let arg1 = ("c").to_owned();
            let arg2 = (trigger).to_owned();
            b.store.call("peers.peer_delivery_command", move |store| store.peer_delivery_command(&arg0, &arg1, &arg2)).await
        }?;
        let again = b.peer_request(&network, &ia.device_id()?,
            json!({"type":"deliver","command":command.envelope})).await?;
        assert_eq!(again["receipt"]["trigger"], trigger.as_str());
        assert_eq!(
            {
                let arg0 = ("c").to_owned();
                a.store.call("peers.scheduler_jobs", move |store| store.scheduler_jobs(&arg0)).await
            }?.iter().filter(|job| job.trigger == trigger).count(),
            1,
            "a repeated delivery returns the original admission"
        );

        let target = {
            let arg0 = (blob.hash).to_owned();
            b.store.call("peers.blob_file", move |store| store.blob_file(&arg0)).await
        }?;
        std::fs::remove_file(&target)?;
        let part = {
            let arg0 = (blob.hash).to_owned();
            b.store.call("peers.partial_blob_file", move |store| store.partial_blob_file(&arg0)).await
        }?;
        let mut seeded = content[..1024 * 1024].to_vec();
        seeded[0] ^= 1;
        seeded.extend_from_slice(b"bytes after the last synced offset");
        std::fs::write(&part, &seeded)?;
        {
            let arg0 = (blob.hash).to_owned();
            let arg1 = Some(1024 * 1024);
            b.store.call("peers.commit_offset", move |store| store.commit_offset(&arg0, arg1)).await
        }?;
        let corrupted = b.fetch_blob(&network, &ia.device_id()?, &blob).await;
        assert!(corrupted.is_err() && !part.exists(), "resumed bytes keep the synced prefix and fail the hash check");
        std::fs::write(&part, &content[..1024 * 1024])?;
        {
            let arg0 = (blob.hash).to_owned();
            let arg1 = Some(1024 * 1024);
            b.store.call("peers.commit_offset", move |store| store.commit_offset(&arg0, arg1)).await
        }?;
        b.fetch_blob(&network, &ia.device_id()?, &blob).await?;
        assert_eq!(std::fs::read(&target)?, content);
        assert!(!part.exists());

        outsider.net()?.learn(&a.net()?.addr()).await;
        let hello = json!({"type":"hello","network_id":network,"vector":{},"membership_epoch":0,"membership":""});
        assert!(outsider.net()?.request(&ia.device_id()?, net::SYNC, &hello, Duration::from_secs(5)).await.is_err());
        assert!(outsider.net()?.request(&ia.device_id()?, net::RPC, &json!({"type":"capabilities","network_id":network}), Duration::from_secs(5)).await.is_err());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish_nodes(result, vec![(a, dir_a), (b, dir_b), (outsider, dir_c)]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn events_that_wait_for_an_ancestor_are_pushed_and_acted_on_once_stored() -> Result<()> {
    let [il, ih, ic] = [
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    ];
    let admin = il.fixture_admin();
    let device = |id: &LocalIdentity| id.device_id().unwrap();
    let state = il.sign_network_fixture(1, vec![device(&il), device(&ih), device(&ic)])?;
    let network = il.fixture_network_id()?;
    let (l, dir_l) = node(&il).await?;
    let (h, dir_h) = node(&ih).await?;
    let (c, dir_c) = node(&ic).await?;
    let result = async {
        for node in [&l, &h, &c] {
            node.install_membership(state.clone()).await?;
        }
        let trigger = {
            let il = il.clone();
            let ic = ic.clone();
            let admin = admin.clone();
            let network = network.clone();
            c.store
                .call("peers.fixture", move |store| {
                    let owner = il.owner_fixture(&network, "bot", &device(&ic))?;
                    store.create_conversation(&ic, &network, "bot", "Bot", owner, now_ms())?;
                    let bot = admin.attest(
                        &network,
                        "bot",
                        EventBody::BotCreated {
                            bot_id: "bot".into(),
                            name: "Bot".into(),
                            instructions: String::new(),
                            project: None,
                            attestation: None,
                            cause: None,
                        },
                    )?;
                    store.append_event(&ic, &network, "bot", bot, vec![], now_ms())?;
                    store.post_input_fixture(
                        &ic,
                        &network,
                        posted(&il, &network, "bot", "m1", 1)?,
                        now_ms(),
                    )?;
                    let claimed = store
                        .claim_next_fixture(&ic, &network, "bot", "boot", "cfg")?
                        .context("job")?;
                    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
                    store.mark_started("bot", &claimed.job.trigger, &attempt)?;
                    Ok(claimed.job.trigger)
                })
                .await?
        };
        let log_of = async |node: &Runtime, id: &LocalIdentity| {
            let arg0 = (network).to_owned();
            let arg1 = (device(id)).to_owned();
            let arg2 = 1;
            let arg3 = 1024;
            node.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        };
        {
            let arg0 = (network).to_owned();
            let arg1 = (log_of(&c, &ic).await?).to_owned();
            l.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        let owner = il.owner_fixture(&network, "s", &device(&il))?;
        {
            let arg0 = (ih).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("s").to_owned();
            let arg3 = ("S").to_owned();
            let arg4 = owner;
            h.store
                .call("peers.create_conversation", move |store| {
                    store.create_conversation(&arg0, &arg1, &arg2, &arg3, arg4, now_ms())
                })
                .await
        }?;
        {
            let ih = ih.clone();
            let admin = admin.clone();
            let network = network.clone();
            h.store
                .call("peers.create_input_bot", move |store| {
                    create_input_bot(store, &ih, &admin, &network, "s", now_ms())
                })
                .await?;
        }
        let from_h = log_of(&h, &ih).await?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (from_h).to_owned();
            l.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        let deleted = admin
            .attest(
                &network,
                "bot",
                EventBody::BotDeleted {
                    attestation: None,
                    cause: None,
                },
            )?
            .attestation()
            .cloned()
            .context("signed deletion")?;
        let (posted, deletion) = {
            let il = il.clone();
            let network = network.clone();
            let deleted = deleted.clone();
            l.store
                .call("peers.fixture", move |store| {
                    let (posted, _) = store.post_input_fixture(
                        &il,
                        &network,
                        posted(&il, &network, "s", "l1", 1)?,
                        now_ms(),
                    )?;
                    let deletion = store
                        .delete_bot(&il, &network, "bot", deleted, now_ms())?
                        .context("deleted")?;
                    Ok((posted.event_hash, deletion.hash))
                })
                .await?
        };

        let mut updates = c.events.subscribe();
        let pushed = |updates: &mut broadcast::Receiver<Update>| {
            std::iter::from_fn(|| updates.try_recv().ok())
                .filter_map(|update| {
                    Some((
                        update.payload["conversation_id"].as_str()?.to_owned(),
                        update.payload["event_hash"].as_str()?.to_owned(),
                    ))
                })
                .collect::<Vec<_>>()
        };
        c.ingest(&network, &log_of(&l, &il).await?).await?;
        assert_eq!(
            {
                let arg0 = (network).to_owned();
                let arg1 = (posted).to_owned();
                c.store
                    .call("peers.event", move |store| store.event(&arg0, &arg1))
                    .await
            }?,
            None,
            "L's events wait for H's"
        );
        assert!(
            pushed(&mut updates).is_empty(),
            "nothing is pushed for an event still waiting"
        );

        c.ingest(&network, &from_h).await?;
        let after = pushed(&mut updates);
        assert!(
            after.contains(&("s".into(), posted.clone())),
            "the message stored with its ancestor is pushed: {after:?}"
        );
        assert!(
            after.contains(&("bot".into(), deletion.clone())),
            "the deletion stored with its ancestor is pushed"
        );
        let stopped = async {
            loop {
                let job = {
                    let arg0 = ("bot").to_owned();
                    let arg1 = (trigger).to_owned();
                    c.store
                        .call("peers.scheduler_job", move |store| {
                            store.scheduler_job(&arg0, &arg1)
                        })
                        .await
                }?
                .context("job")?;
                if !matches!(job.state, crate::scheduling::JobState::Running { .. }) {
                    return Ok::<_, anyhow::Error>(job.state);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let state = tokio::time::timeout(Duration::from_secs(10), stopped)
            .await
            .context("the deleted bot's running turn is stopped")??;
        assert!(
            matches!(
                state,
                crate::scheduling::JobState::Cancelling { .. }
                    | crate::scheduling::JobState::Cancelled { .. }
            ),
            "{state:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop((l, h, c));
    for dir in [dir_l, dir_h, dir_c] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

#[tokio::test]
async fn every_local_append_wakes_the_have_notice() -> Result<()> {
    let identity = LocalIdentity::generate();
    let state = identity.sign_network_fixture(1, vec![identity.device_id()?])?;
    let network = identity.fixture_network_id()?;
    let (node, dir) = node(&identity).await?;
    let result = async {
        node.install_membership(state).await?;
        let owner = identity.owner_fixture(&network, "c", &identity.device_id()?)?;
        let woken = node.have_wakeup.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        {
            let arg0 = (identity).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("c").to_owned();
            let arg3 = ("C").to_owned();
            let arg4 = owner;
            node.store
                .call("peers.create_conversation", move |store| {
                    store.create_conversation(&arg0, &arg1, &arg2, &arg3, arg4, now_ms())
                })
                .await
        }?;
        tokio::time::timeout(Duration::from_secs(1), woken)
            .await
            .context("an append that publishes nothing still announces the new head")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(node);
    let _ = std::fs::remove_dir_all(dir);
    result
}
