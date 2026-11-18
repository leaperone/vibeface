//! A private, opt-in courier for newly signed events of one conversation.

use super::{
    Delivery,
    session::{SessionRoute, checked_event, kind_for_event},
    session_outbox_store::SessionOutboxStore,
    types::parse_word,
};
use crate::{clock, model::IngestStatus, runtime::Runtime};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use vibeface_delivery::hash;

const BUNDLE_LIFETIME_MS: u64 = 12 * 60 * 60 * 1000;
const MAX_BUNDLE_ATTACHMENT_BYTES: u64 = 120 * 1024 * 1024;
const MAX_BUNDLE_ATTACHMENTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEndpointRoute {
    pub conversation_id: String,
    pub peer: String,
    pub relay: String,
}

impl SessionEndpointRoute {
    pub(super) fn validate(&self) -> Result<()> {
        vibeface_protocol::validate_id(&self.conversation_id).map_err(anyhow::Error::msg)?;
        ensure!(
            parse_word(&self.peer)? != parse_word(&self.relay)?,
            "session recipient and relay must be different Nodes"
        );
        Ok(())
    }
}

impl Delivery {
    pub(super) fn attach_session_outbox(&self, runtime: std::sync::Weak<Runtime>) -> Result<()> {
        let Some(node) = runtime.upgrade() else {
            return Ok(());
        };
        let mut changes = self.changes.subscribe();
        node.spawn_background(async move {
            loop {
                let Some(node) = runtime.upgrade() else {
                    return;
                };
                let config = node.delivery.config();
                if !(config.enabled && config.charity_enabled && config.session_route.is_some()) {
                    drop(node);
                    if changes.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                let work = async {
                    if node.has_membership().await? {
                        let network = node.membership_state().await?.genesis.network_id;
                        if let Err(error) = node.delivery.run_session_outbox(&node, &network).await
                        {
                            tracing::debug!(error=%error, "session relay upload is pending");
                        }
                        node.delivery.run_session_inbox(&node, &network).await?;
                    }
                    Ok::<(), anyhow::Error>(())
                };
                tokio::select! {
                    result = work => {
                        if let Err(error) = result {
                            tracing::debug!(error=%error, "session relay download is pending");
                        }
                    }
                    result = changes.changed() => {
                        if result.is_err() { return; }
                        continue;
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                    result = changes.changed() => if result.is_err() { return; },
                }
            }
        })
    }

    async fn run_session_inbox(&self, node: &Arc<Runtime>, network: &str) -> Result<()> {
        let config = self.config();
        let Some(route) = config
            .session_route
            .filter(|_| config.enabled && config.charity_enabled)
        else {
            return Ok(());
        };
        let network_id = network.to_owned();
        let peer = route.peer.clone();
        node.store
            .call("session_inbox.authorize", move |store| {
                store.authorize_node(&network_id, &peer)?;
                Ok(())
            })
            .await?;
        for id in self.session_inbox(node, &route).await? {
            if let Err(error) = self.receive_session_bundle(node, network, &route, id).await {
                tracing::debug!(error=%error, "session relay object is pending");
            }
        }
        Ok(())
    }

    async fn receive_session_bundle(
        &self,
        node: &Arc<Runtime>,
        network: &str,
        route: &SessionEndpointRoute,
        id: vibeface_delivery::Word,
    ) -> Result<()> {
        let relay = parse_word(&route.relay)?;
        let (object, capsule) = self
            .download_session_object_limited(
                node,
                relay,
                id,
                crate::model::MAX_EVENT_BYTES * 2 + 32 * 1024,
            )
            .await?;
        ensure!(
            object.header.sender_peer == parse_word(&route.peer)?
                && object.header.conversation_id == route.conversation_id,
            "session inbox object belongs to another route"
        );
        let object = Arc::new(object);
        let open_event = || {
            let (node, network, object) =
                (Arc::clone(node), network.to_owned(), Arc::clone(&object));
            let worker = node.clone();
            async move {
                node.store
                    .call("session_inbox.ingest", move |store| {
                        worker.delivery.ingest_session_event(
                            store,
                            &network,
                            &object,
                            &capsule,
                            clock::now_ms_u64(),
                        )
                    })
                    .await
            }
        };
        let mut received = open_event().await?;
        if received.sync.rebuilt || !received.sync.stored.is_empty() {
            node.publish_reread(network);
        }
        match &received.sync.statuses[0] {
            IngestStatus::Accepted | IngestStatus::Duplicate => {}
            IngestStatus::Pending => return Ok(()),
            _ => anyhow::bail!("session inbox event did not pass normal admission"),
        }
        let mut attachment_ids = Vec::new();
        if !received.attachments.is_empty() {
            let mut total = 0u64;
            ensure!(
                received.attachments.len() <= MAX_BUNDLE_ATTACHMENTS,
                "too many session attachments"
            );
            for offer in received.attachments {
                total = total
                    .checked_add(offer.blob.size)
                    .context("session attachment size overflow")?;
                ensure!(
                    total <= MAX_BUNDLE_ATTACHMENT_BYTES,
                    "session attachment bundle exceeds its resource limit"
                );
                let mut parts = Vec::new();
                for (index, id) in offer.objects.iter().copied().enumerate() {
                    let size = super::session_attachment::part_size(&offer.blob, index)?;
                    parts.push(
                        self.download_session_object_limited(
                            node,
                            relay,
                            id,
                            size + 64 + 12 + vibeface_delivery::SESSION_AEAD_TAG_BYTES,
                        )
                        .await?,
                    );
                    attachment_ids.push(id);
                }
                let (network, event_hash, worker) = (
                    network.to_owned(),
                    received.event_hash.clone(),
                    node.clone(),
                );
                let (parts, receipts) = node
                    .store
                    .call("session_inbox.attachment", move |store| {
                        let receipts = worker.delivery.ingest_session_attachment(
                            store,
                            &network,
                            &event_hash,
                            &offer.blob,
                            &parts,
                            clock::now_ms_u64(),
                        )?;
                        Ok((parts, receipts))
                    })
                    .await?;
                for ((part, _), receipt) in parts.iter().zip(receipts) {
                    self.send_session_receipt(node, part, receipt).await?;
                }
            }
            received = open_event().await?;
        }
        if let Some(receipt) = received.stored_receipt {
            self.acknowledge_session_object(node, &object, receipt)
                .await?;
            // Retain attachment caches until the parent receipt succeeds so retries need no new chunks.
            self.clear_session_downloads(&object.header.receiver_peer, attachment_ids);
        }
        Ok(())
    }

    async fn run_session_outbox(&self, node: &Runtime, network: &str) -> Result<()> {
        let config = self.config();
        let Some(route) = config
            .session_route
            .filter(|_| config.enabled && config.charity_enabled)
        else {
            return Ok(());
        };
        let local = self.identity.device_id()?;
        ensure!(
            local != route.peer && local != route.relay,
            "session route points back to this Node"
        );
        let route_id = hex::encode(hash(&serde_json::to_vec(&(network, &local, &route))?));
        let (network_id, sender, peer) = (network.to_owned(), local.clone(), route.peer.clone());
        let head = node
            .store
            .call("session_outbox.authorize", move |store| {
                store.authorize_node(&network_id, &sender)?;
                store.authorize_node(&network_id, &peer)?;
                Ok(store
                    .heads(&network_id)?
                    .get(&sender)
                    .map_or(0, |head| head.0))
            })
            .await?;
        let (cursor, existing) = {
            let mut outbox = SessionOutboxStore::open(&self.data_dir)?;
            let Some(cursor) = outbox.cursor(&route_id)? else {
                // Enabling the route does not export messages already present in this log.
                outbox.initialize(&route_id, head)?;
                return Ok(());
            };
            (cursor, outbox.batch(&route_id)?)
        };

        let (sequence, objects) = if let Some(batch) = existing {
            (batch.sequence, batch.objects)
        } else {
            let network_id = network.to_owned();
            let conversation = route.conversation_id.clone();
            let next = node
                .store
                .read("session_outbox.next", move |store| {
                    let Some(signed) = store
                        .events_from(&network_id, &local, cursor.saturating_add(1), 1)?
                        .pop()
                    else {
                        return Ok(None);
                    };
                    let event = crate::crypto::check_event(&network_id, &signed)?;
                    let effective: bool = store.connection.query_row(
                        "SELECT effective FROM events WHERE network_id=? AND hash=?",
                        rusqlite::params![network_id, signed.hash],
                        |row| row.get(0),
                    )?;
                    if !effective
                        || event.scope_id != conversation
                        || kind_for_event(&event.body).is_err()
                    {
                        return Ok(Some((event.seq, None)));
                    }
                    checked_event(&network_id, &signed)?;
                    let total = event
                        .blobs
                        .iter()
                        .try_fold(0u64, |sum, blob| sum.checked_add(blob.size));
                    ensure!(
                        event.blobs.len() <= MAX_BUNDLE_ATTACHMENTS
                            && total.is_some_and(|size| size <= MAX_BUNDLE_ATTACHMENT_BYTES),
                        "session attachment bundle exceeds its resource limit"
                    );
                    let files = event
                        .blobs
                        .iter()
                        .map(|blob| {
                            let (reference, bytes) =
                                store.read_blob(&network_id, &event.scope_id, &blob.hash)?;
                            ensure!(
                                &reference == blob,
                                "session file does not match the signed reference"
                            );
                            Ok((reference, bytes))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Some((event.seq, Some((signed, files)))))
                })
                .await?;
            let Some((sequence, source)) = next else {
                return Ok(());
            };
            let Some((signed, files)) = source else {
                SessionOutboxStore::open(&self.data_dir)?.advance(&route_id, cursor, sequence)?;
                return Ok(());
            };
            let at = clock::now_ms_u64();
            let binding = SessionRoute {
                network: network.into(),
                recipient: parse_word(&route.peer)?,
                relay: parse_word(&route.relay)?,
                validity: at.saturating_sub(5_000)..at.saturating_add(BUNDLE_LIFETIME_MS),
            };
            let objects = self
                .seal_session_bundle(node, &binding, &signed, &files)
                .await?;
            SessionOutboxStore::open(&self.data_dir)?
                .stage(&route_id, cursor, sequence, &objects)?;
            (sequence, objects)
        };
        for (object, capsule) in &objects {
            // A paused or changed route cannot continue sending its prepared objects.
            let current = self.config();
            ensure!(
                current.enabled
                    && current.charity_enabled
                    && current.session_route.as_ref() == Some(&route),
                "session route is no longer enabled"
            );
            self.upload_session_object(node, object, capsule).await?;
        }
        // This is a transport cursor; it is not receipt evidence or authorization to charge.
        SessionOutboxStore::open(&self.data_dir)?.advance(&route_id, cursor, sequence)?;
        Ok(())
    }
}
