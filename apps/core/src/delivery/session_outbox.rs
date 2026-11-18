//! A private, opt-in courier for newly signed events of one conversation.

use super::{
    Delivery,
    chain::{Call, Chain, TxObservation},
    session::{CharityKey, SessionRoute, checked_event, kind_for_event},
    session_outbox_store::SessionOutboxStore,
    types::parse_word,
};
use crate::{clock, model::IngestStatus, runtime::Runtime};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use vibeface_session_delivery::{Amount, SessionSettlementClaim, SessionSettlementOrder, hash};

const BUNDLE_LIFETIME_MS: u64 = 12 * 60 * 60 * 1000;
const MAX_BUNDLE_ATTACHMENT_BYTES: u64 = 120 * 1024 * 1024;
const MAX_BUNDLE_ATTACHMENTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPaymentConfig {
    pub deployment: String,
    pub budget: String,
    pub amount: String,
    pub claim_until_ms: u64,
}

impl SessionPaymentConfig {
    pub(super) fn validate(&self) -> Result<()> {
        parse_word(&self.deployment)?;
        parse_word(&self.budget)?;
        ensure!(
            self.claim_until_ms > 0,
            "session payment claim deadline is empty"
        );
        Amount::from_decimal_str(&self.amount).map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

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
    fn paid_session_claim(
        route_id: &str,
        sequence: u64,
        payment: &SessionPaymentConfig,
        header: &vibeface_session_delivery::SessionObjectHeader,
        receipt: &super::session::SessionStoredReceipt,
    ) -> Result<SessionSettlementClaim> {
        let deployment = parse_word(&payment.deployment)?;
        let budget = parse_word(&payment.budget)?;
        let amount = Amount::from_decimal_str(&payment.amount).map_err(anyhow::Error::msg)?;
        let delivery = hash(&serde_json::to_vec(&(route_id, sequence))?);
        let order = SessionSettlementOrder {
            deployment,
            budget,
            delivery,
            receiver: header.receiver_peer,
            relay: header.provider_peer,
            object: header.object_id,
            amount,
            claim_until_ms: payment.claim_until_ms,
        };
        let receipt = receipt.signature_bytes()?;
        order.commitment().map_err(anyhow::Error::msg)?;
        Ok(SessionSettlementClaim { order, receipt })
    }

    pub fn paid_session_claims(&self, route_id: &str) -> Result<Vec<SessionSettlementClaim>> {
        SessionOutboxStore::open(&self.data_dir)?.pending_claims(route_id, 16)
    }

    /// Submit one already-verified claim batch through the existing durable chain transaction path.
    /// The caller supplies the genuine Groth16 seal; this method never treats a queued claim as paid.
    pub async fn settle_paid_session_batch(
        &self,
        chain: &Chain,
        route_id: &str,
        seal: Vec<u8>,
        gas_cap: &str,
    ) -> Result<TxObservation> {
        let claims = self.paid_session_claims(route_id)?;
        ensure!(!claims.is_empty(), "session payment claim queue is empty");
        let batch = vibeface_session_delivery::verify_session_settlement(&claims)
            .map_err(anyhow::Error::msg)?;
        let ids = claims
            .iter()
            .map(|claim| claim.order.order_id().map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()?;
        let intent_id = format!(
            "session-settlement/{route_id}/{}",
            hex::encode(hash(&batch.journal()))
        );
        chain
            .execute(
                self,
                &intent_id,
                Call::SessionSettle {
                    budget: batch.budget,
                    order_ids: ids,
                    seal,
                },
                gas_cap,
            )
            .await
    }

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
        id: vibeface_session_delivery::Word,
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
                            size + 64 + 12 + vibeface_session_delivery::SESSION_AEAD_TAG_BYTES,
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

    async fn outbox_receiving_key(
        &self,
        node: &Runtime,
        route_id: &str,
        peer: &str,
        binding: &SessionRoute,
    ) -> Result<CharityKey> {
        let cached = SessionOutboxStore::open(&self.data_dir)?.receiving_key(route_id)?;
        if let Some(key) = cached
            && key
                .session_material(&binding.recipient, binding.validity.clone())
                .is_ok()
        {
            return Ok(key);
        }
        // Key unavailability must not hold the independent inbox behind a network timeout.
        let key = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Self::session_receiving_key(node, peer),
        )
        .await
        .context("waiting for the recipient's signed session key")??;
        key.session_material(&binding.recipient, binding.validity.clone())?;
        SessionOutboxStore::open(&self.data_dir)?.save_receiving_key(route_id, &key)?;
        Ok(key)
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

        let pending = SessionOutboxStore::open(&self.data_dir)?
            .next_receipt(&route_id, clock::now_ms_u64())?;
        if let Some((sequence, header)) = pending {
            match self.fetch_session_receipt(node, &header).await {
                Ok(Some(receipt)) => {
                    let outbox = SessionOutboxStore::open(&self.data_dir)?;
                    outbox.record_receipt(&route_id, sequence, &receipt)?;
                    if let Some(payment) = self.config().session_payment.clone() {
                        let claim = Self::paid_session_claim(
                            &route_id, sequence, &payment, &header, &receipt,
                        )?;
                        outbox.enqueue_claim(&route_id, sequence, &claim)?;
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "session relay receipt is pending"),
            }
        }

        let (sequence, objects) = if let Some(batch) = existing {
            (batch.sequence, batch.objects)
        } else {
            let at = clock::now_ms_u64();
            let binding = SessionRoute {
                network: network.into(),
                recipient: parse_word(&route.peer)?,
                relay: parse_word(&route.relay)?,
                validity: at.saturating_sub(5_000)..at.saturating_add(BUNDLE_LIFETIME_MS),
            };
            // Warm the signed public key even before the first event, while the peer is reachable.
            let key = self
                .outbox_receiving_key(node, &route_id, &route.peer, &binding)
                .await;
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
                return key.map(|_| ());
            };
            let Some((signed, files)) = source else {
                SessionOutboxStore::open(&self.data_dir)?.advance(&route_id, cursor, sequence)?;
                return Ok(());
            };
            let objects = self.seal_session_bundle(&binding, &key?, &signed, &files)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use vibeface_session_delivery::{SessionObjectHeader, SessionObjectKind};

    #[test]
    fn paid_claim_binds_the_verified_object_and_has_a_stable_retry_id() -> Result<()> {
        let payment = SessionPaymentConfig {
            deployment: "01".repeat(32),
            budget: "02".repeat(32),
            amount: "17".into(),
            claim_until_ms: 99,
        };
        let header = SessionObjectHeader {
            version: 1,
            kind: SessionObjectKind::Message,
            object_id: [7; 32],
            session_id: "network".into(),
            conversation_id: "conversation".into(),
            sequence: 1,
            previous_object_id: [6; 32],
            sender_peer: [3; 32],
            receiver_peer: [4; 32],
            provider_peer: [5; 32],
            payload_hash: [8; 32],
            payload_bytes: 100,
            created_at_ms: 1,
            expires_at_ms: 1000,
            chunk_count: 1,
        };
        let receipt =
            super::super::session::SessionStoredReceipt::for_test([7; 32], hex::encode([9; 64]));
        let first = Delivery::paid_session_claim("route", 3, &payment, &header, &receipt)?;
        let retry = Delivery::paid_session_claim("route", 3, &payment, &header, &receipt)?;
        assert_eq!(first, retry);
        assert_eq!(first.order.amount, Amount::from_u64(17));
        assert_eq!(first.order.object, header.object_id);
        assert!(
            Delivery::paid_session_claim("route", 4, &payment, &header, &receipt)
                .map(|claim| claim.order.delivery != first.order.delivery)
                .unwrap_or(false)
        );
        Ok(())
    }
}
