use super::{Delivery, now, random_word, service, store::ServiceStore, types::*};
use crate::runtime::Runtime;
use alloy_primitives::B256;
use alloy_sol_types::SolValue;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_delivery::{
    Amount, Secret32, WindowRandomness, WindowSpec, Word, encrypt_charity_window, hash,
    receiving_public, recover_charity_window, validate_receiving_public,
};
use zeroize::Zeroizing;

const MAX_WINDOWS: usize = 2;
const WINDOW_BYTES: usize = 65_536;
const ACCEPT_SECONDS: u64 = 3600;
const RETAIN_SECONDS: u64 = 86400;
const PEER_ORDERS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CharityKey {
    receiver_peer: String,
    public_key: String,
    key_id: String,
    serial: u64,
    valid_from: u64,
    valid_until: u64,
    signature: String,
}

impl CharityKey {
    fn payload(&self) -> Result<Vec<u8>> {
        ensure!(
            self.serial > 0 && self.valid_from < self.valid_until,
            "invalid charity receiving key interval"
        );
        let public = parse_word(&self.public_key)?;
        let key_id = parse_word(&self.key_id)?;
        validate_receiving_public(&public)?;
        ensure!(
            public != [0; 32] && key_id != [0; 32],
            "empty charity receiving key"
        );
        Ok((
            tag("vibeface-charity-key-v1"),
            word(&self.receiver_peer)?,
            B256::from(public),
            B256::from(key_id),
            self.serial,
            self.valid_from,
            self.valid_until,
        )
            .abi_encode_params())
    }

    fn verify(&self) -> Result<()> {
        vibeface_secure::verify(
            &self.receiver_peer,
            "delivery-charity-key",
            &self.payload()?,
            &self.signature,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CharityTerms {
    order_id: String,
    order_nonce: String,
    intent_id: String,
    source_peer: String,
    receiver_peer: String,
    provider_peer: String,
    receiving_key: CharityKey,
    file_bytes: u64,
    envelope_bytes: u64,
    created_at: u64,
    accept_until: u64,
    retain_until: u64,
}

impl CharityTerms {
    fn expected_id(&self) -> Result<String> {
        Ok(hex::encode(hash(
            &(
                tag("vibeface-charity-order-v1"),
                word(&self.source_peer)?,
                word(&self.order_nonce)?,
            )
                .abi_encode_params(),
        )))
    }

    fn payload(&self) -> Result<Vec<u8>> {
        self.receiving_key.verify()?;
        valid_intent(&self.intent_id)?;
        let source = word(&self.source_peer)?;
        let receiver = word(&self.receiver_peer)?;
        let provider = word(&self.provider_peer)?;
        ensure!(
            source != receiver && source != provider && receiver != provider,
            "charity delivery requires distinct sending, receiving and forwarding Nodes"
        );
        ensure!(
            self.receiving_key.receiver_peer == self.receiver_peer,
            "charity receiving key belongs to another Node"
        );
        ensure!(
            word(&self.order_nonce)? != B256::ZERO && self.expected_id()? == self.order_id,
            "charity order identifier does not match its source and nonce"
        );
        ensure!(
            self.file_bytes <= MAX_FILE_BYTES as u64
                && self.envelope_bytes >= self.file_bytes + 43
                && self.envelope_bytes <= self.file_bytes + 170
                && self.envelope_bytes <= (MAX_WINDOWS * WINDOW_BYTES) as u64,
            "charity file or encrypted envelope exceeds its bounds"
        );
        ensure!(
            self.created_at > 0
                && self.accept_until > self.created_at
                && self.accept_until <= self.created_at.saturating_add(ACCEPT_SECONDS)
                && self.retain_until >= self.accept_until
                && self.retain_until <= self.created_at.saturating_add(RETAIN_SECONDS)
                && self.receiving_key.valid_from <= self.created_at
                && self.receiving_key.valid_until >= self.retain_until,
            "invalid charity acceptance or retention deadline"
        );
        Ok((
            tag("vibeface-charity-terms-v1"),
            word(&self.order_id)?,
            word(&self.order_nonce)?,
            B256::from(hash(self.intent_id.as_bytes())),
            source,
            receiver,
            provider,
            B256::from(hash(&self.receiving_key.payload()?)),
            self.file_bytes,
            self.envelope_bytes,
            self.created_at,
            self.accept_until,
            self.retain_until,
        )
            .abi_encode_params())
    }

    fn context(&self) -> Result<Word> {
        Ok(hash(&self.payload()?))
    }

    fn active_offer(&self, at: u64) -> Result<()> {
        self.payload()?;
        ensure!(
            self.created_at <= at.saturating_add(60) && at <= self.accept_until,
            "charity offer is not currently valid"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CharityOrder {
    terms: CharityTerms,
    windows: Vec<WindowSpec>,
    signature: String,
}

impl CharityOrder {
    fn manifest(&self) -> Result<Vec<u8>> {
        let context = self.terms.context()?;
        ensure!(
            !self.windows.is_empty() && self.windows.len() <= MAX_WINDOWS,
            "invalid charity window count"
        );
        let mut offset = 0u64;
        let mut leaves = Vec::with_capacity(self.windows.len());
        for (index, spec) in self.windows.iter().enumerate() {
            ensure!(
                spec.index as usize == index
                    && spec.offset == offset
                    && spec.plaintext_bytes > 0
                    && spec.plaintext_bytes as usize <= WINDOW_BYTES
                    && spec.ciphertext_bytes == spec.plaintext_bytes + 68
                    && spec.delivery_fee == Amount::ZERO
                    && spec.reservation_fee == Amount::ZERO
                    && spec.ciphertext_hash != [0; 32]
                    && spec.capsule_hash != [0; 32]
                    && spec.plaintext_commitment != [0; 32],
                "charity manifest has invalid coverage, fees or object bounds"
            );
            offset = offset
                .checked_add(u64::from(spec.plaintext_bytes))
                .context("charity window offset overflow")?;
            leaves.push(B256::from(hash(
                &(
                    tag("vibeface-charity-window-v1"),
                    spec.index,
                    spec.offset,
                    spec.plaintext_bytes,
                    spec.ciphertext_bytes,
                    B256::from(spec.ciphertext_hash),
                    B256::from(spec.capsule_hash),
                    B256::from(spec.plaintext_commitment),
                )
                    .abi_encode_params(),
            )));
        }
        ensure!(
            offset == self.terms.envelope_bytes,
            "charity windows do not cover the exact file envelope"
        );
        Ok((
            tag("vibeface-charity-manifest-v1"),
            B256::from(context),
            leaves,
        )
            .abi_encode_params())
    }

    fn digest(&self) -> Result<Word> {
        Ok(hash(&self.manifest()?))
    }

    fn verify(&self) -> Result<()> {
        vibeface_secure::verify(
            &self.terms.source_peer,
            "delivery-charity-source",
            &self.manifest()?,
            &self.signature,
        )?;
        Ok(())
    }

    fn consent(&self) -> Result<Vec<u8>> {
        Ok((
            tag("vibeface-charity-consent-v1"),
            word(&self.terms.order_id)?,
            B256::from(self.digest()?),
            word(&self.terms.receiver_peer)?,
            word(&self.terms.receiving_key.key_id)?,
            self.terms.accept_until,
        )
            .abi_encode_params())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CharityReceipt {
    order_id: String,
    manifest_hash: String,
    receiver_peer: String,
    key_id: String,
    received_at: u64,
    signature: String,
}

impl CharityReceipt {
    fn payload(&self) -> Result<Vec<u8>> {
        Ok((
            tag("vibeface-charity-received-v1"),
            word(&self.order_id)?,
            word(&self.manifest_hash)?,
            word(&self.receiver_peer)?,
            word(&self.key_id)?,
            self.received_at,
        )
            .abi_encode_params())
    }

    fn verify(&self, order: &CharityOrder, at: u64) -> Result<()> {
        ensure!(
            self.order_id == order.terms.order_id
                && self.manifest_hash == hex::encode(order.digest()?)
                && self.receiver_peer == order.terms.receiver_peer
                && self.key_id == order.terms.receiving_key.key_id
                && self.received_at >= order.terms.created_at
                && self.received_at <= order.terms.retain_until
                && self.received_at <= at.saturating_add(60),
            "charity receipt does not describe this delivery"
        );
        vibeface_secure::verify(
            &self.receiver_peer,
            "delivery-charity-received",
            &self.payload()?,
            &self.signature,
        )?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceivingMaterial {
    key: CharityKey,
    secret: Secret32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceDraft {
    network: String,
    request_hash: String,
    request: Prepare,
    terms: CharityTerms,
    randomness: Vec<WindowRandomness>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCharity {
    network: Option<String>,
    authorized_by: Option<String>,
    request_hash: String,
    view: DeliveryOrder,
    order: CharityOrder,
    confirmed: bool,
    accepted: bool,
    notified: bool,
    resume_requested: bool,
    receipt: Option<CharityReceipt>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Announcement {
    peer_id: String,
    max_file_bytes: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    order_id: String,
    manifest_hash: String,
    receipt: Option<CharityReceipt>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Announce,
    Key,
    Reserve {
        order: CharityOrder,
    },
    Offer {
        order: CharityOrder,
    },
    Offset {
        order_id: String,
        index: u16,
        operation: GrantOperation,
    },
    Put {
        order_id: String,
        index: u16,
        operation: GrantOperation,
        offset: u32,
        bytes: String,
    },
    Get {
        order_id: String,
        index: u16,
        operation: GrantOperation,
        offset: u32,
    },
    Accept {
        order_id: String,
        signature: String,
    },
    Receipt {
        receipt: CharityReceipt,
    },
    Order {
        order_id: String,
    },
}

impl Delivery {
    fn charity_key(&self) -> Result<CharityKey> {
        self.ensure_mode(Mode::Charity)?;
        self.with_store(|store| {
            let previous = store
                .value("charity-receiving-current")?
                .map(|bytes| serde_json::from_slice::<ReceivingMaterial>(&bytes))
                .transpose()?;
            if let Some(material) = &previous {
                material.key.verify()?;
                ensure!(
                    receiving_public(&material.secret)? == parse_word(&material.key.public_key)?,
                    "stored charity receiving material is inconsistent"
                );
                if material.key.valid_until > now().saturating_add(2 * RETAIN_SECONDS) {
                    return Ok(material.key.clone());
                }
            }
            let secret = Secret32::new(random_word())?;
            let mut key = CharityKey {
                receiver_peer: self.identity.device_id()?,
                public_key: hex::encode(receiving_public(&secret)?),
                key_id: hex::encode(random_word()),
                serial: match previous {
                    Some(material) => material
                        .key
                        .serial
                        .checked_add(1)
                        .context("charity receiving key serial exhausted")?,
                    None => 1,
                },
                valid_from: now().saturating_sub(60),
                valid_until: now().saturating_add(7 * RETAIN_SECONDS),
                signature: String::new(),
            };
            key.signature = self
                .identity
                .sign_payload("delivery-charity-key", &key.payload()?)?;
            let material = ReceivingMaterial {
                key: key.clone(),
                secret,
            };
            let bytes = Zeroizing::new(serde_json::to_vec(&material)?);
            store.put_value(&format!("charity-receiving:{}", key.key_id), &bytes)?;
            store.put_value("charity-receiving-current", &bytes)?;
            Ok(key)
        })
    }

    fn charity_secret(&self, key: &CharityKey) -> Result<Secret32> {
        key.verify()?;
        ensure!(
            key.receiver_peer == self.identity.device_id()?,
            "charity receiving key belongs to another Node"
        );
        let bytes = Zeroizing::new(
            self.with_store(|store| store.value(&format!("charity-receiving:{}", key.key_id)))?
                .context(
                    "charity receiving key is unavailable; restore the independent delivery backup",
                )?,
        );
        let material: ReceivingMaterial = serde_json::from_slice(&bytes)?;
        ensure!(
            material.key == *key
                && receiving_public(&material.secret)? == parse_word(&key.public_key)?,
            "charity receiving key material does not match the order"
        );
        Ok(material.secret)
    }

    pub(super) fn charity_key_status(&self) -> Result<Value> {
        let key = self.charity_key()?;
        Ok(json!({"key_id":key.key_id,"serial":key.serial,"public_key":key.public_key}))
    }

    fn charity_stored(&self, id: &str) -> Result<StoredCharity> {
        let encoded = self
            .with_store(|store| store.charity_order(id))?
            .context("charity order not found")?;
        let stored: StoredCharity = serde_json::from_str(&encoded)?;
        stored.order.verify()?;
        ensure!(
            stored.view.order_id == id && stored.view.mode == Mode::Charity,
            "stored charity order has inconsistent identity"
        );
        Ok(stored)
    }

    fn charity_scoped(&self, network: &str, id: &str) -> Result<StoredCharity> {
        let stored = self.charity_stored(id)?;
        ensure!(
            stored.network.as_deref() == Some(network),
            "charity order belongs to another personal network"
        );
        Ok(stored)
    }

    fn charity_save(&self, stored: &mut StoredCharity) -> Result<()> {
        self.with_store(|store| save_record(store, stored))
    }

    pub(super) fn charity_get(&self, network: &str, id: &str) -> Result<DeliveryOrder> {
        Ok(self.charity_scoped(network, id)?.view)
    }

    pub(super) fn charity_find_intent(
        &self,
        network: &str,
        intent: &str,
    ) -> Result<Option<DeliveryOrder>> {
        valid_intent(intent)?;
        self.with_store(|store| store.charity_by_intent(network, intent))?
            .map(|body| {
                let stored: StoredCharity = serde_json::from_str(&body)?;
                ensure!(
                    stored.network.as_deref() == Some(network) && stored.view.role == Role::Sender,
                    "charity intent belongs to another owner or role"
                );
                Ok(stored.view)
            })
            .transpose()
    }

    pub(super) fn charity_list(&self, network: &str) -> Result<Vec<DeliveryOrder>> {
        self.with_store(|store| store.charity_orders(Some(network)))?
            .into_iter()
            .map(|body| {
                let stored: StoredCharity = serde_json::from_str(&body)?;
                ensure!(
                    stored.network.as_deref() == Some(network) && stored.view.mode == Mode::Charity,
                    "charity list crossed a personal network boundary"
                );
                Ok(stored.view)
            })
            .collect()
    }

    pub(super) async fn charity_prepare(
        &self,
        node: &Runtime,
        network: &str,
        request: Prepare,
    ) -> Result<DeliveryOrder> {
        self.ensure_mode(Mode::Charity)?;
        ensure!(
            request.mode == Mode::Charity && request.acceptance == Acceptance::Recoverable,
            "charity delivery uses signed receipts and does not offer strict proof acceptance"
        );
        valid_intent(&request.intent_id)?;
        file_name(&request.name)?;
        ensure!(
            request.name.trim() == request.name && !request.name.contains(':'),
            "invalid charity file name"
        );
        parse_word(&request.receiver_peer)?;
        parse_word(&request.provider_peer)?;
        Amount::from_decimal_str(&request.max_service_amount)?;
        Amount::from_decimal_str(&request.max_gas_amount)?;
        ensure!(
            request.content_base64.len() <= 4 * MAX_FILE_BYTES.div_ceil(3),
            "charity file exceeds its input bound"
        );
        let content = Zeroizing::new(STANDARD.decode(&request.content_base64)?);
        ensure!(
            content.len() <= MAX_FILE_BYTES && STANDARD.encode(&*content) == request.content_base64,
            "charity file encoding is not canonical or bounded"
        );
        let request_hash = hex::encode(hash(&Zeroizing::new(serde_json::to_vec(&request)?)));
        ensure!(
            self.with_store(|store| store.by_intent(network, &request.intent_id))?
                .is_none(),
            "this intent was already used by a paid delivery"
        );
        if let Some(body) =
            self.with_store(|store| store.charity_by_intent(network, &request.intent_id))?
        {
            let stored: StoredCharity = serde_json::from_str(&body)?;
            ensure!(
                stored.view.role == Role::Sender && stored.request_hash == request_hash,
                "charity intent changed across retry"
            );
            return Ok(stored.view);
        }
        let draft_key = format!(
            "charity-draft:{}",
            hex::encode(hash(&(network, &request.intent_id).abi_encode_params()))
        );
        let draft = match self.with_store(|store| store.value(&draft_key))? {
            Some(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let draft: SourceDraft = serde_json::from_slice(&bytes)?;
                ensure!(
                    draft.network == network && draft.request_hash == request_hash,
                    "charity draft changed across retry"
                );
                draft
            }
            None => {
                let key: CharityKey = serde_json::from_value(
                    charity_request(node, &request.receiver_peer, Request::Key).await?,
                )?;
                key.verify()?;
                ensure!(
                    key.receiver_peer == request.receiver_peer,
                    "charity key response changed the receiver"
                );
                let announcement: Announcement = serde_json::from_value(
                    charity_request(node, &request.provider_peer, Request::Announce).await?,
                )?;
                ensure!(
                    announcement.peer_id == request.provider_peer
                        && announcement.max_file_bytes >= content.len(),
                    "charity forwarder does not accept this file"
                );
                let envelope = private_file(&request.name, &content)?;
                let at = now();
                let mut terms = CharityTerms {
                    order_id: String::new(),
                    order_nonce: hex::encode(random_word()),
                    intent_id: request.intent_id.clone(),
                    source_peer: self.identity.device_id()?,
                    receiver_peer: request.receiver_peer.clone(),
                    provider_peer: request.provider_peer.clone(),
                    receiving_key: key,
                    file_bytes: content.len() as u64,
                    envelope_bytes: envelope.len() as u64,
                    created_at: at,
                    accept_until: at.saturating_add(ACCEPT_SECONDS),
                    retain_until: at.saturating_add(RETAIN_SECONDS),
                };
                terms.order_id = terms.expected_id()?;
                terms.active_offer(at)?;
                let count = envelope.len().div_ceil(WINDOW_BYTES);
                let randomness = (0..count)
                    .map(|_| {
                        Ok(WindowRandomness {
                            salt: Secret32::new(random_word())?,
                            data_key: Secret32::new(random_word())?,
                            nonce: rand::random(),
                            hpke_seed: Secret32::new(random_word())?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let draft = SourceDraft {
                    network: network.to_owned(),
                    request_hash: request_hash.clone(),
                    request,
                    terms,
                    randomness,
                };
                let encoded = Zeroizing::new(serde_json::to_vec(&draft)?);
                self.with_store(|store| store.put_value(&draft_key, &encoded))?;
                draft
            }
        };
        draft.terms.active_offer(now())?;
        ensure!(
            draft.terms.source_peer == self.identity.device_id()?,
            "charity draft belongs to another source Node"
        );
        let envelope = Zeroizing::new(private_file(&draft.request.name, &content)?);
        ensure!(
            envelope.len() as u64 == draft.terms.envelope_bytes
                && draft.randomness.len() == envelope.len().div_ceil(WINDOW_BYTES),
            "charity draft has inconsistent window material"
        );
        self.with_store(|store| {
            store.put_value(&format!("charity-file:{}", draft.terms.order_id), &envelope)
        })?;
        let context = draft.terms.context()?;
        let mut windows = Vec::new();
        for (index, plaintext) in envelope.chunks(WINDOW_BYTES).enumerate() {
            let encrypted = encrypt_charity_window(
                context,
                parse_word(&draft.terms.receiving_key.public_key)?,
                index as u16,
                (index * WINDOW_BYTES) as u64,
                plaintext,
                &draft.randomness[index],
            )?;
            self.with_store(|store| {
                store.write_object(
                    &draft.terms.order_id,
                    index as u16,
                    "ciphertext",
                    &encrypted.ciphertext,
                )?;
                store.write_object(
                    &draft.terms.order_id,
                    index as u16,
                    "capsule",
                    &encrypted.capsule,
                )
            })?;
            windows.push(encrypted.spec);
        }
        let mut order = CharityOrder {
            terms: draft.terms,
            windows,
            signature: String::new(),
        };
        order.signature = self
            .identity
            .sign_payload("delivery-charity-source", &order.manifest()?)?;
        order.verify()?;
        let mut stored = new_record(order, Role::Sender, Some(network.to_owned()), request_hash);
        stored.view.name = Some(draft.request.name);
        stored.view.sha256 = Some(hex::encode(hash(&content)));
        self.charity_save(&mut stored)?;
        Ok(stored.view)
    }

    pub(super) async fn charity_confirm(
        &self,
        _node: &Runtime,
        network: &str,
        intent: &str,
        device: &str,
    ) -> Result<DeliveryOrder> {
        valid_intent(intent)?;
        let body = self
            .with_store(|store| store.charity_by_intent(network, intent))?
            .context("prepared charity intent not found")?;
        let mut stored: StoredCharity = serde_json::from_str(&body)?;
        ensure!(
            stored.network.as_deref() == Some(network) && stored.view.role == Role::Sender,
            "only the original source Node can confirm this charity intent"
        );
        self.ensure_mode(Mode::Charity)?;
        stored.order.verify()?;
        parse_word(device)?;
        stored.authorized_by = Some(device.to_owned());
        if !stored.confirmed {
            stored.order.terms.active_offer(now())?;
            stored.confirmed = true;
        }
        self.charity_schedule(&mut stored)?;
        Ok(stored.view)
    }

    pub(super) async fn charity_action(
        &self,
        _node: &Runtime,
        network: &str,
        id: &str,
        accept: bool,
        device: &str,
    ) -> Result<DeliveryOrder> {
        let mut stored = self.charity_scoped(network, id)?;
        self.ensure_mode(Mode::Charity)?;
        parse_word(device)?;
        stored.authorized_by = Some(device.to_owned());
        if accept {
            ensure!(
                stored.view.role == Role::Receiver,
                "only the receiving Node can accept charity delivery"
            );
            if !stored.accepted {
                stored.order.terms.active_offer(now())?;
                ensure!(
                    self.charity_key()?.key_id == stored.order.terms.receiving_key.key_id,
                    "charity receiving key rotated before acceptance"
                );
                self.charity_secret(&stored.order.terms.receiving_key)?;
                stored.accepted = true;
            }
        }
        self.charity_schedule(&mut stored)?;
        Ok(stored.view)
    }

    fn charity_schedule(&self, stored: &mut StoredCharity) -> Result<()> {
        stored.resume_requested = match stored.view.role {
            Role::Sender => stored.confirmed && stored.receipt.is_none(),
            Role::Receiver => stored.accepted && !stored.notified,
            Role::Provider => false,
        };
        if stored.resume_requested {
            ensure!(
                stored.network.is_some() && stored.authorized_by.is_some(),
                "charity transfer has no managing member authority"
            );
            stored.view.phase = if stored.receipt.is_some() {
                Phase::Recoverable
            } else {
                Phase::Transferring
            };
            stored.view.error = None;
        }
        self.charity_save(stored)?;
        if stored.resume_requested {
            self.wakeup.notify_one();
        }
        Ok(())
    }

    pub(super) async fn charity_run_authorized(&self, node: &Runtime) -> Result<()> {
        let config = self.config();
        if !config.enabled || !config.charity_enabled || !node.has_membership().await? {
            return Ok(());
        }
        let network = node.membership_state().await?.genesis.network_id;
        let bodies = self.with_store(|store| store.charity_orders(Some(&network)))?;
        let mut pending = Vec::new();
        for body in bodies {
            let stored: StoredCharity = serde_json::from_str(&body)?;
            ensure!(
                stored.network.as_deref() == Some(network.as_str()),
                "charity executor crossed a personal network boundary"
            );
            let authorized = match stored.view.role {
                Role::Sender => stored.confirmed,
                Role::Receiver => stored.accepted,
                Role::Provider => false,
            };
            if stored.resume_requested && authorized {
                pending.push(stored);
            }
        }
        let Some(mut stored) = pending
            .into_iter()
            .min_by_key(|stored| stored.view.updated_at_ms)
        else {
            return Ok(());
        };
        stored.order.verify()?;
        if let Err(error) = self.charity_progress(node, &mut stored).await {
            stored.resume_requested = false;
            stored.view.phase = if stored.receipt.is_some() {
                Phase::Recoverable
            } else {
                Phase::Partial
            };
            stored.view.error = Some(error.to_string().chars().take(512).collect());
        } else {
            stored.view.error = None;
            stored.resume_requested = match stored.view.role {
                Role::Sender => stored.receipt.is_none(),
                Role::Receiver => !stored.notified,
                Role::Provider => false,
            };
        }
        self.charity_save(&mut stored)
    }

    async fn charity_authorized(&self, stored: &StoredCharity) -> Result<()> {
        self.authorize_effect(
            stored
                .network
                .as_deref()
                .context("charity effect has no personal network owner")?,
            stored
                .authorized_by
                .as_deref()
                .context("charity effect has no managing member authority")?,
        )
        .await
    }

    async fn charity_progress(&self, node: &Runtime, stored: &mut StoredCharity) -> Result<()> {
        self.ensure_mode(Mode::Charity)?;
        self.charity_authorized(stored).await?;
        match stored.view.role {
            Role::Sender => {
                if !stored.confirmed {
                    return Ok(());
                }
                let provider = stored.view.provider_peer.clone();
                let prior = charity_request(
                    node,
                    &provider,
                    Request::Order {
                        order_id: stored.view.order_id.clone(),
                    },
                )
                .await?;
                if !prior.is_null() {
                    let prior: Snapshot = serde_json::from_value(prior)?;
                    ensure!(
                        prior.order_id == stored.view.order_id
                            && prior.manifest_hash == hex::encode(stored.order.digest()?),
                        "charity provider returned another order"
                    );
                    if let Some(receipt) = prior.receipt {
                        receipt.verify(&stored.order, now())?;
                        stored.receipt = Some(receipt);
                        mark_received(stored);
                        return Ok(());
                    }
                }
                if !stored.notified {
                    self.charity_authorized(stored).await?;
                    charity_request(
                        node,
                        &provider,
                        Request::Reserve {
                            order: stored.order.clone(),
                        },
                    )
                    .await?;
                    for index in 0..stored.order.windows.len() {
                        self.charity_authorized(stored).await?;
                        for operation in [GrantOperation::PutCiphertext, GrantOperation::PutCapsule]
                        {
                            self.charity_upload(node, stored, index as u16, operation)
                                .await?;
                        }
                        stored.view.windows[index].committed_bytes =
                            stored.order.windows[index].ciphertext_bytes;
                        stored.view.windows[index].state = WindowState::Stored;
                        self.charity_save(stored)?;
                    }
                    self.charity_authorized(stored).await?;
                    charity_request(
                        node,
                        &stored.view.receiver_peer,
                        Request::Offer {
                            order: stored.order.clone(),
                        },
                    )
                    .await?;
                    stored.notified = true;
                    self.charity_save(stored)?;
                }
            }
            Role::Receiver => {
                if !stored.accepted {
                    return Ok(());
                }
                if stored.receipt.is_none() {
                    let consent = self
                        .identity
                        .sign_payload("delivery-charity-accept", &stored.order.consent()?)?;
                    charity_request(
                        node,
                        &stored.view.provider_peer,
                        Request::Accept {
                            order_id: stored.view.order_id.clone(),
                            signature: consent,
                        },
                    )
                    .await?;
                    let secret = self.charity_secret(&stored.order.terms.receiving_key)?;
                    let mut envelope = Zeroizing::new(Vec::with_capacity(
                        stored.order.terms.envelope_bytes as usize,
                    ));
                    for index in 0..stored.order.windows.len() {
                        self.charity_authorized(stored).await?;
                        for operation in [GrantOperation::GetCiphertext, GrantOperation::GetCapsule]
                        {
                            self.charity_download(node, stored, index as u16, operation)
                                .await?;
                        }
                        let (ciphertext, capsule) = self.with_store(|store| {
                            Ok((
                                store.object(&stored.view.order_id, index as u16, "ciphertext")?,
                                store.object(&stored.view.order_id, index as u16, "capsule")?,
                            ))
                        })?;
                        let recovered = recover_charity_window(
                            stored.order.terms.context()?,
                            parse_word(&stored.order.terms.receiving_key.public_key)?,
                            &stored.order.windows[index],
                            &secret,
                            &ciphertext,
                            &capsule,
                        )?;
                        envelope.extend_from_slice(&recovered.plaintext);
                        stored.view.windows[index].committed_bytes =
                            stored.order.windows[index].ciphertext_bytes;
                        stored.view.windows[index].state = WindowState::Stored;
                        self.charity_save(stored)?;
                    }
                    ensure!(
                        envelope.len() as u64 == stored.order.terms.envelope_bytes,
                        "charity recovered envelope length changed"
                    );
                    let (name, content) =
                        checked_private_file(&envelope, stored.order.terms.file_bytes)?;
                    stored.view.name = Some(name.to_owned());
                    stored.view.sha256 = Some(hex::encode(hash(content)));
                    self.with_store(|store| {
                        store
                            .put_value(&format!("charity-file:{}", stored.view.order_id), &envelope)
                    })?;
                    let mut receipt = CharityReceipt {
                        order_id: stored.view.order_id.clone(),
                        manifest_hash: hex::encode(stored.order.digest()?),
                        receiver_peer: self.identity.device_id()?,
                        key_id: stored.order.terms.receiving_key.key_id.clone(),
                        received_at: now(),
                        signature: String::new(),
                    };
                    receipt.signature = self
                        .identity
                        .sign_payload("delivery-charity-received", &receipt.payload()?)?;
                    receipt.verify(&stored.order, now())?;
                    stored.receipt = Some(receipt);
                    mark_received(stored);
                    self.charity_save(stored)?;
                }
                self.charity_read(
                    stored
                        .network
                        .as_deref()
                        .context("receiving charity order has no owner")?,
                    &stored.view.order_id,
                )?;
                self.charity_authorized(stored).await?;
                charity_request(
                    node,
                    &stored.view.provider_peer,
                    Request::Receipt {
                        receipt: stored
                            .receipt
                            .clone()
                            .context("charity receipt is unavailable")?,
                    },
                )
                .await?;
                stored.notified = true;
            }
            Role::Provider => {
                bail!("public charity forwarding is reconciled through its service grants")
            }
        }
        Ok(())
    }

    pub(super) fn charity_read(&self, network: &str, id: &str) -> Result<Value> {
        let stored = self.charity_scoped(network, id)?;
        ensure!(
            stored.view.role == Role::Sender
                || stored.view.role == Role::Receiver
                    && stored.accepted
                    && stored.receipt.is_some(),
            "charity file is not available to this Node"
        );
        let bytes = Zeroizing::new(
            self.with_store(|store| store.value(&format!("charity-file:{id}")))?
                .context("charity file has not been recovered locally")?,
        );
        let (name, content) = checked_private_file(&bytes, stored.order.terms.file_bytes)?;
        let digest = hex::encode(hash(content));
        ensure!(
            stored
                .view
                .name
                .as_deref()
                .is_none_or(|expected| expected == name)
                && stored
                    .view
                    .sha256
                    .as_deref()
                    .is_none_or(|expected| expected == digest),
            "charity file metadata or digest differs from the order"
        );
        Ok(
            json!({"name":name,"content_base64":STANDARD.encode(content),"sha256":digest,"bytes":content.len()}),
        )
    }

    async fn charity_upload(
        &self,
        node: &Runtime,
        stored: &StoredCharity,
        index: u16,
        operation: GrantOperation,
    ) -> Result<()> {
        self.charity_authorized(stored).await?;
        let provider = &stored.view.provider_peer;
        let (grant, kind) = authorize(
            &stored.order,
            &self.identity.device_id()?,
            index,
            operation.clone(),
            false,
            now(),
        )?;
        let bytes = self.with_store(|store| store.object(&stored.view.order_id, index, kind))?;
        ensure!(
            hex::encode(hash(&bytes)) == grant.object_hash
                && bytes.len() == grant.max_bytes as usize,
            "charity local encrypted object differs from its signed manifest"
        );
        let mut offset = offset_reply(
            charity_request(
                node,
                provider,
                Request::Offset {
                    order_id: stored.view.order_id.clone(),
                    index,
                    operation: operation.clone(),
                },
            )
            .await?,
            grant.max_bytes,
        )?;
        while offset < grant.max_bytes {
            self.ensure_mode(Mode::Charity)?;
            self.charity_authorized(stored).await?;
            let end = (offset as usize + CHUNK_BYTES).min(bytes.len());
            let next = offset_reply(
                charity_request(
                    node,
                    provider,
                    Request::Put {
                        order_id: stored.view.order_id.clone(),
                        index,
                        operation: operation.clone(),
                        offset,
                        bytes: STANDARD.encode(&bytes[offset as usize..end]),
                    },
                )
                .await?,
                grant.max_bytes,
            )?;
            ensure!(
                next == end as u32,
                "charity upload did not acknowledge the exact durable prefix"
            );
            offset = next;
        }
        Ok(())
    }

    async fn charity_download(
        &self,
        node: &Runtime,
        stored: &mut StoredCharity,
        index: u16,
        operation: GrantOperation,
    ) -> Result<()> {
        self.charity_authorized(stored).await?;
        let provider = stored.view.provider_peer.clone();
        let (grant, kind) = authorize(
            &stored.order,
            &self.identity.device_id()?,
            index,
            operation.clone(),
            true,
            now(),
        )?;
        let mut offset = self.with_store(|store| {
            store.reserve_object(
                &stored.view.order_id,
                index,
                kind,
                &grant.object_hash,
                grant.max_bytes,
            )
        })?;
        while offset < grant.max_bytes {
            self.ensure_mode(Mode::Charity)?;
            self.charity_authorized(stored).await?;
            let value = charity_request(
                node,
                &provider,
                Request::Get {
                    order_id: stored.view.order_id.clone(),
                    index,
                    operation: operation.clone(),
                    offset,
                },
            )
            .await?;
            let encoded = value["bytes"]
                .as_str()
                .context("charity download omitted its encrypted bytes")?;
            ensure!(
                encoded.len() <= 4 * CHUNK_BYTES.div_ceil(3),
                "charity encrypted chunk exceeds its limit"
            );
            let bytes = STANDARD.decode(encoded)?;
            ensure!(
                !bytes.is_empty()
                    && bytes.len() <= CHUNK_BYTES
                    && STANDARD.encode(&bytes) == encoded,
                "invalid charity encrypted chunk"
            );
            let next = self.with_store(|store| {
                store.append_object(&stored.view.order_id, index, kind, offset, &bytes)
            })?;
            ensure!(
                offset_reply(value, grant.max_bytes)? == next,
                "charity download changed its offset"
            );
            offset = next;
            if kind == "ciphertext" {
                stored.view.windows[index as usize].committed_bytes = offset;
                self.charity_save(stored)?;
            }
        }
        Ok(())
    }

    fn charity_provider(&self, id: &str) -> Result<StoredCharity> {
        let peer = self.identity.device_id()?;
        self.with_store(|store| provider_record(store, id, &peer))
    }

    pub(super) async fn charity_handle_service(
        &self,
        node: &Runtime,
        remote: &str,
        request: Value,
    ) -> Result<Value> {
        self.ensure_mode(Mode::Charity)?;
        let request: Request = serde_json::from_value(request)?;
        match request {
            Request::Announce => Ok(serde_json::to_value(Announcement {
                peer_id: self.identity.device_id()?,
                max_file_bytes: MAX_FILE_BYTES,
            })?),
            Request::Key => Ok(serde_json::to_value(self.charity_key()?)?),
            Request::Reserve { order } => {
                order.verify()?;
                order.terms.active_offer(now())?;
                let peer = self.identity.device_id()?;
                ensure!(
                    order.terms.provider_peer == peer && order.terms.source_peer == remote,
                    "charity reservation has another provider or source"
                );
                self.with_store(|store| {
                    let stored = if let Some(body) = store.charity_order(&order.terms.order_id)? {
                        let stored: StoredCharity = serde_json::from_str(&body)?;
                        ensure!(
                            stored.order == order
                                && stored.network.is_none()
                                && stored.view.role == Role::Provider,
                            "charity reservation changed across retry"
                        );
                        stored
                    } else {
                        capacity(store, None, remote)?;
                        let mut stored = new_record(order, Role::Provider, None, String::new());
                        save_record(store, &mut stored)?;
                        stored
                    };
                    for spec in &stored.order.windows {
                        store.reserve_object(
                            &stored.view.order_id,
                            spec.index,
                            "ciphertext",
                            &hex::encode(spec.ciphertext_hash),
                            spec.ciphertext_bytes,
                        )?;
                        store.reserve_object(
                            &stored.view.order_id,
                            spec.index,
                            "capsule",
                            &hex::encode(spec.capsule_hash),
                            80,
                        )?;
                    }
                    snapshot(&stored)
                })
            }
            Request::Offer { order } => {
                order.verify()?;
                ensure!(
                    order.terms.receiver_peer == self.identity.device_id()?
                        && order.terms.source_peer == remote,
                    "charity offer has another receiving or sending Node"
                );
                let network = node.membership_state().await?.genesis.network_id;
                if self
                    .with_store(|store| store.charity_order(&order.terms.order_id))?
                    .is_none()
                {
                    order.terms.active_offer(now())?;
                    ensure!(
                        self.charity_key()?.key_id == order.terms.receiving_key.key_id,
                        "charity receiving key rotated before the offer"
                    );
                }
                self.with_store(|store| {
                    if let Some(body) = store.charity_order(&order.terms.order_id)? {
                        let stored: StoredCharity = serde_json::from_str(&body)?;
                        ensure!(
                            stored.order == order
                                && stored.view.role == Role::Receiver
                                && stored.network.as_deref() == Some(network.as_str()),
                            "charity offer changed owner or content across retry"
                        );
                        return Ok(json!({"order_id":stored.view.order_id}));
                    }
                    capacity(store, Some(&network), remote)?;
                    let mut stored =
                        new_record(order, Role::Receiver, Some(network), String::new());
                    save_record(store, &mut stored)?;
                    Ok(json!({"order_id":stored.view.order_id}))
                })
            }
            Request::Offset {
                order_id,
                index,
                operation,
            } => {
                ensure!(
                    matches!(
                        operation,
                        GrantOperation::PutCiphertext | GrantOperation::PutCapsule
                    ),
                    "charity offset does not permit reading"
                );
                let stored = self.charity_provider(&order_id)?;
                let (grant, kind) = authorize(
                    &stored.order,
                    remote,
                    index,
                    operation,
                    stored.accepted,
                    now(),
                )?;
                let offset = self.with_store(|store| {
                    store.reserve_object(
                        &order_id,
                        index,
                        kind,
                        &grant.object_hash,
                        grant.max_bytes,
                    )
                })?;
                Ok(json!({"offset":offset}))
            }
            Request::Put {
                order_id,
                index,
                operation,
                offset,
                bytes,
            } => {
                ensure!(
                    matches!(
                        operation,
                        GrantOperation::PutCiphertext | GrantOperation::PutCapsule
                    ),
                    "charity operation does not permit upload"
                );
                let stored = self.charity_provider(&order_id)?;
                let (_, kind) = authorize(
                    &stored.order,
                    remote,
                    index,
                    operation,
                    stored.accepted,
                    now(),
                )?;
                ensure!(
                    bytes.len() <= 4 * CHUNK_BYTES.div_ceil(3),
                    "charity upload chunk exceeds its limit"
                );
                let data = STANDARD.decode(&bytes)?;
                ensure!(
                    STANDARD.encode(&data) == bytes,
                    "charity upload chunk is not canonical base64"
                );
                let next = self.with_store(|store| {
                    store.append_object(&order_id, index, kind, offset, &data)
                })?;
                Ok(json!({"offset":next}))
            }
            Request::Get {
                order_id,
                index,
                operation,
                offset,
            } => {
                ensure!(
                    matches!(
                        operation,
                        GrantOperation::GetCiphertext | GrantOperation::GetCapsule
                    ),
                    "charity operation does not permit reading"
                );
                let stored = self.charity_provider(&order_id)?;
                let (_, kind) = authorize(
                    &stored.order,
                    remote,
                    index,
                    operation,
                    stored.accepted,
                    now(),
                )?;
                let bytes = self.with_store(|store| store.object(&order_id, index, kind))?;
                ensure!(
                    (offset as usize) < bytes.len(),
                    "charity read offset exceeds the object"
                );
                let end = (offset as usize + CHUNK_BYTES).min(bytes.len());
                Ok(json!({"bytes":STANDARD.encode(&bytes[offset as usize..end]),"offset":end}))
            }
            Request::Accept {
                order_id,
                signature,
            } => {
                let peer = self.identity.device_id()?;
                self.with_store(|store| {
                    let mut stored = provider_record(store, &order_id, &peer)?;
                    ensure!(
                        remote == stored.order.terms.receiver_peer,
                        "only the designated receiving Node can consent to charity delivery"
                    );
                    vibeface_secure::verify(
                        remote,
                        "delivery-charity-accept",
                        &stored.order.consent()?,
                        &signature,
                    )?;
                    if !stored.accepted {
                        stored.order.terms.active_offer(now())?;
                        stored.accepted = true;
                        save_record(store, &mut stored)?;
                    }
                    snapshot(&stored)
                })
            }
            Request::Receipt { receipt } => {
                let peer = self.identity.device_id()?;
                self.with_store(|store| {
                    let mut stored = provider_record(store, &receipt.order_id, &peer)?;
                    ensure!(
                        remote == stored.order.terms.receiver_peer && stored.accepted,
                        "charity receipt requires the designated receiver's prior consent"
                    );
                    receipt.verify(&stored.order, now())?;
                    if let Some(prior) = &stored.receipt {
                        ensure!(*prior == receipt, "charity receipt changed across retry");
                    }
                    for spec in &stored.order.windows {
                        store.object(&stored.view.order_id, spec.index, "ciphertext")?;
                        store.object(&stored.view.order_id, spec.index, "capsule")?;
                    }
                    stored.receipt = Some(receipt);
                    mark_received(&mut stored);
                    save_record(store, &mut stored)?;
                    snapshot(&stored)
                })
            }
            Request::Order { order_id } => {
                if self
                    .with_store(|store| store.charity_order(&order_id))?
                    .is_none()
                {
                    return Ok(Value::Null);
                }
                let stored = self.charity_provider(&order_id)?;
                ensure!(
                    remote == stored.order.terms.source_peer
                        || remote == stored.order.terms.receiver_peer,
                    "charity order belongs to other Nodes"
                );
                snapshot(&stored)
            }
        }
    }
}

async fn charity_request(node: &Runtime, peer: &str, request: Request) -> Result<Value> {
    service::request(
        node,
        peer,
        &ServiceRequest::Charity {
            request: serde_json::to_value(request)?,
        },
    )
    .await
}

fn save_record(store: &mut ServiceStore, stored: &mut StoredCharity) -> Result<()> {
    stored.view.updated_at_ms = crate::runtime::now_ms();
    let intent = if stored.view.role == Role::Sender {
        stored.view.intent_id.clone()
    } else {
        format!(
            "incoming:{}:{}",
            stored.view.source_peer, stored.view.intent_id
        )
    };
    store.save_charity_order(
        &stored.view.order_id,
        stored.network.as_deref(),
        &intent,
        &serde_json::to_string(stored)?,
    )
}

fn provider_record(store: &ServiceStore, id: &str, provider: &str) -> Result<StoredCharity> {
    let encoded = store
        .charity_order(id)?
        .context("charity provider order not found")?;
    let stored: StoredCharity = serde_json::from_str(&encoded)?;
    stored.order.verify()?;
    ensure!(
        stored.view.order_id == id
            && stored.view.mode == Mode::Charity
            && stored.network.is_none()
            && stored.view.role == Role::Provider
            && stored.order.terms.provider_peer == provider,
        "charity order is not hosted by this provider"
    );
    Ok(stored)
}

fn capacity(store: &ServiceStore, network: Option<&str>, source: &str) -> Result<()> {
    let mut count = 0;
    for body in store.charity_orders(network)? {
        let prior: StoredCharity = serde_json::from_str(&body)?;
        if prior.view.source_peer == source
            && prior.receipt.is_none()
            && prior.order.terms.retain_until >= now()
        {
            count += 1;
        }
    }
    ensure!(
        count < PEER_ORDERS,
        "charity order budget for this sending Node is exhausted"
    );
    Ok(())
}

fn new_record(
    order: CharityOrder,
    role: Role,
    network: Option<String>,
    request_hash: String,
) -> StoredCharity {
    let at = crate::runtime::now_ms();
    let view = DeliveryOrder {
        order_id: order.terms.order_id.clone(),
        intent_id: order.terms.intent_id.clone(),
        role,
        mode: Mode::Charity,
        acceptance: Acceptance::Recoverable,
        source_peer: order.terms.source_peer.clone(),
        receiver_peer: order.terms.receiver_peer.clone(),
        provider_peer: order.terms.provider_peer.clone(),
        name: None,
        bytes: order.terms.file_bytes,
        sha256: None,
        phase: Phase::Prepared,
        evidence: Evidence::None,
        payment: Payment::free(),
        windows: order
            .windows
            .iter()
            .map(|spec| WindowView {
                index: spec.index,
                bytes: spec.plaintext_bytes,
                committed_bytes: 0,
                state: WindowState::Pending,
                decryption_proven: false,
            })
            .collect(),
        deadlines: None,
        created_at_ms: at,
        updated_at_ms: at,
        error: None,
    };
    StoredCharity {
        network,
        authorized_by: None,
        request_hash,
        view,
        order,
        confirmed: false,
        accepted: false,
        notified: false,
        resume_requested: false,
        receipt: None,
    }
}

fn mark_received(stored: &mut StoredCharity) {
    stored.view.evidence = Evidence::CharityReceipt;
    stored.view.phase = Phase::Recoverable;
    for (window, spec) in stored.view.windows.iter_mut().zip(&stored.order.windows) {
        window.committed_bytes = spec.ciphertext_bytes;
        window.state = WindowState::Held;
        window.decryption_proven = false;
    }
}

fn snapshot(stored: &StoredCharity) -> Result<Value> {
    Ok(serde_json::to_value(Snapshot {
        order_id: stored.view.order_id.clone(),
        manifest_hash: hex::encode(stored.order.digest()?),
        receipt: stored.receipt.clone(),
    })?)
}

fn authorize(
    order: &CharityOrder,
    remote: &str,
    index: u16,
    operation: GrantOperation,
    accepted: bool,
    at: u64,
) -> Result<(GrantBody, &'static str)> {
    order.verify()?;
    let spec = order
        .windows
        .get(index as usize)
        .context("charity window index exceeds its manifest")?;
    let uploading = matches!(
        operation,
        GrantOperation::PutCiphertext | GrantOperation::PutCapsule
    );
    ensure!(
        remote
            == if uploading {
                &order.terms.source_peer
            } else {
                &order.terms.receiver_peer
            },
        "charity grant client has the wrong role"
    );
    ensure!(
        at <= if uploading {
            order.terms.accept_until
        } else {
            order.terms.retain_until
        },
        "charity object authorization expired"
    );
    ensure!(
        uploading || accepted,
        "receiving Node has not consented to this charity delivery"
    );
    let capsule = matches!(
        operation,
        GrantOperation::PutCapsule | GrantOperation::GetCapsule
    );
    Ok((
        GrantBody {
            provider_peer: order.terms.provider_peer.clone(),
            client_peer: remote.to_owned(),
            order_id: order.terms.order_id.clone(),
            window_index: index,
            object_hash: hex::encode(if capsule {
                spec.capsule_hash
            } else {
                spec.ciphertext_hash
            }),
            operation,
            max_bytes: if capsule { 80 } else { spec.ciphertext_bytes },
            expires_at: if uploading {
                order.terms.accept_until
            } else {
                order.terms.retain_until
            },
        },
        if capsule { "capsule" } else { "ciphertext" },
    ))
}

fn offset_reply(value: Value, limit: u32) -> Result<u32> {
    let offset = value["offset"]
        .as_u64()
        .context("charity response omitted its durable offset")?;
    ensure!(
        offset <= u64::from(limit),
        "charity offset exceeds the reserved object"
    );
    Ok(offset as u32)
}

fn private_file(name: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut result = b"VDC1".to_vec();
    result.extend_from_slice(&hash(bytes));
    result.extend_from_slice(&envelope(name, bytes)?);
    ensure!(
        result.len() <= MAX_WINDOWS * WINDOW_BYTES,
        "charity private file envelope exceeds two windows"
    );
    Ok(result)
}

fn checked_private_file(bytes: &[u8], expected_bytes: u64) -> Result<(&str, &[u8])> {
    ensure!(
        bytes.len() >= 43 && &bytes[..4] == b"VDC1",
        "invalid charity private file envelope"
    );
    let (name, content) = open_envelope(&bytes[36..])?;
    ensure!(
        content.len() as u64 == expected_bytes && hash(content).as_slice() == &bytes[4..36],
        "charity original file hash or length differs from its encrypted envelope"
    );
    ensure!(
        name.trim() == name && !name.contains(':'),
        "invalid recovered charity file name"
    );
    Ok((name, content))
}

fn valid_intent(value: &str) -> Result<()> {
    let id = uuid::Uuid::parse_str(value).context("invalid charity intent identifier")?;
    ensure!(
        id.to_string() == value,
        "charity intent must be a canonical lowercase UUID"
    );
    Ok(())
}

fn tag(value: &str) -> B256 {
    B256::from(hash(value.as_bytes()))
}
fn word(value: &str) -> Result<B256> {
    Ok(B256::from(parse_word(value)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::LocalIdentity;

    type Fixture = (
        CharityOrder,
        Secret32,
        Vec<(Vec<u8>, Vec<u8>)>,
        Vec<u8>,
        LocalIdentity,
        LocalIdentity,
    );

    fn fixture() -> Fixture {
        let source = LocalIdentity::generate();
        let receiver = LocalIdentity::generate();
        let provider = LocalIdentity::generate();
        let secret = Secret32::new([7; 32]).unwrap();
        let mut key = CharityKey {
            receiver_peer: receiver.device_id().unwrap(),
            public_key: hex::encode(receiving_public(&secret).unwrap()),
            key_id: "31".repeat(32),
            serial: 1,
            valid_from: 900,
            valid_until: 100_000,
            signature: String::new(),
        };
        key.signature = receiver
            .sign_payload("delivery-charity-key", &key.payload().unwrap())
            .unwrap();
        let content = vec![0x64; 65_536];
        let envelope = private_file("private.bin", &content).unwrap();
        let mut terms = CharityTerms {
            order_id: String::new(),
            order_nonce: "23".repeat(32),
            intent_id: "4d58a9f7-a7b6-414c-94d8-86aa0e816844".into(),
            source_peer: source.device_id().unwrap(),
            receiver_peer: receiver.device_id().unwrap(),
            provider_peer: provider.device_id().unwrap(),
            receiving_key: key,
            file_bytes: content.len() as u64,
            envelope_bytes: envelope.len() as u64,
            created_at: 1000,
            accept_until: 4600,
            retain_until: 87_400,
        };
        terms.order_id = terms.expected_id().unwrap();
        let mut windows = Vec::new();
        let mut objects = Vec::new();
        for (index, plain) in envelope.chunks(WINDOW_BYTES).enumerate() {
            let random = WindowRandomness {
                salt: Secret32::new([10 + index as u8; 32]).unwrap(),
                data_key: Secret32::new([20 + index as u8; 32]).unwrap(),
                nonce: [30 + index as u8; 12],
                hpke_seed: Secret32::new([40 + index as u8; 32]).unwrap(),
            };
            let encrypted = encrypt_charity_window(
                terms.context().unwrap(),
                parse_word(&terms.receiving_key.public_key).unwrap(),
                index as u16,
                (index * WINDOW_BYTES) as u64,
                plain,
                &random,
            )
            .unwrap();
            windows.push(encrypted.spec);
            objects.push((encrypted.ciphertext, encrypted.capsule));
        }
        let mut order = CharityOrder {
            terms,
            windows,
            signature: String::new(),
        };
        order.signature = source
            .sign_payload("delivery-charity-source", &order.manifest().unwrap())
            .unwrap();
        (order, secret, objects, content, receiver, source)
    }

    #[test]
    fn charity_two_windows_recover_private_metadata_and_original_hash_without_financial_terms() {
        let (order, secret, objects, content, _, _) = fixture();
        order.verify().unwrap();
        assert_eq!(order.windows.len(), 2);
        let mut plain = Vec::new();
        for (spec, (ciphertext, capsule)) in order.windows.iter().zip(objects) {
            let recovered = recover_charity_window(
                order.terms.context().unwrap(),
                parse_word(&order.terms.receiving_key.public_key).unwrap(),
                spec,
                &secret,
                &ciphertext,
                &capsule,
            )
            .unwrap();
            plain.extend_from_slice(&recovered.plaintext);
        }
        assert_eq!(
            checked_private_file(&plain, content.len() as u64).unwrap(),
            ("private.bin", content.as_slice())
        );
        plain[4] ^= 1;
        assert!(checked_private_file(&plain, content.len() as u64).is_err());
        let record = new_record(order, Role::Provider, None, String::new());
        let view = serde_json::to_value(record.view).unwrap();
        assert_eq!(view["payment"]["state"], "free");
        assert_eq!(view["payment"]["total"], "0");
        assert!(view["payment"]["payer"].is_null() && view["payment"]["asset"].is_null());
        assert!(view["name"].is_null() && view["sha256"].is_null());
    }

    #[test]
    fn charity_manifest_rejects_changed_identity_gaps_fees_and_wrong_signing_domain() {
        let (order, _, _, _, _, source) = fixture();
        let mut changed = order.clone();
        changed.windows[1].offset += 1;
        assert!(changed.verify().is_err());
        let mut changed = order.clone();
        changed.windows[0].delivery_fee = Amount::from_u64(1);
        assert!(changed.verify().is_err());
        let mut changed = order.clone();
        changed.terms.provider_peer = changed.terms.source_peer.clone();
        assert!(changed.verify().is_err());
        let mut changed = order.clone();
        changed.terms.retain_until += 1;
        assert!(changed.verify().is_err());
        let mut changed = order.clone();
        changed.signature = source
            .sign_payload("delivery-manifest", &order.manifest().unwrap())
            .unwrap();
        assert!(changed.verify().is_err());
        let mut changed = order;
        changed.terms.receiving_key.signature = "00".repeat(64);
        assert!(changed.verify().is_err());
    }

    #[test]
    fn charity_grants_bind_role_consent_object_quota_and_deadline() {
        let (order, _, _, _, _, _) = fixture();
        let (grant, kind) = authorize(
            &order,
            &order.terms.receiver_peer,
            0,
            GrantOperation::GetCiphertext,
            true,
            1001,
        )
        .unwrap();
        assert_eq!(kind, "ciphertext");
        assert_eq!(
            grant.object_hash,
            hex::encode(order.windows[0].ciphertext_hash)
        );
        assert_eq!(grant.max_bytes, order.windows[0].ciphertext_bytes);
        assert_eq!(grant.provider_peer, order.terms.provider_peer);
        assert!(
            authorize(
                &order,
                &order.terms.receiver_peer,
                0,
                GrantOperation::GetCiphertext,
                false,
                1001
            )
            .is_err()
        );
        assert!(
            authorize(
                &order,
                &order.terms.source_peer,
                0,
                GrantOperation::GetCiphertext,
                true,
                1001
            )
            .is_err()
        );
        assert!(
            authorize(
                &order,
                &order.terms.receiver_peer,
                0,
                GrantOperation::GetCiphertext,
                true,
                order.terms.retain_until + 1
            )
            .is_err()
        );
        assert!(
            authorize(
                &order,
                &order.terms.source_peer,
                0,
                GrantOperation::PutCiphertext,
                false,
                order.terms.accept_until + 1
            )
            .is_err()
        );
        assert!(
            authorize(
                &order,
                &order.terms.receiver_peer,
                0,
                GrantOperation::PutCiphertext,
                true,
                1001
            )
            .is_err()
        );
        assert!(
            authorize(
                &order,
                &order.terms.receiver_peer,
                2,
                GrantOperation::GetCiphertext,
                true,
                1001
            )
            .is_err()
        );
    }

    #[test]
    fn charity_receipt_is_a_receiver_signature_bound_to_the_exact_manifest() {
        let (order, _, _, _, receiver, _) = fixture();
        let mut receipt = CharityReceipt {
            order_id: order.terms.order_id.clone(),
            manifest_hash: hex::encode(order.digest().unwrap()),
            receiver_peer: receiver.device_id().unwrap(),
            key_id: order.terms.receiving_key.key_id.clone(),
            received_at: 2000,
            signature: String::new(),
        };
        receipt.signature = receiver
            .sign_payload("delivery-charity-received", &receipt.payload().unwrap())
            .unwrap();
        receipt.verify(&order, 2000).unwrap();
        let mut changed = receipt.clone();
        changed.manifest_hash = "18".repeat(32);
        assert!(changed.verify(&order, 2000).is_err());
        let mut changed = receipt;
        changed.received_at = order.terms.retain_until + 1;
        assert!(
            changed
                .verify(&order, order.terms.retain_until + 1)
                .is_err()
        );
    }

    #[test]
    fn charity_executor_requires_explicit_authority_and_creates_no_wallet() {
        let (order, _, _, _, _, source) = fixture();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/charity-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir).unwrap();
        let delivery = Delivery::open(&dir, source).unwrap();
        delivery
            .configure_local(super::super::DeliveryConfig {
                enabled: true,
                ..Default::default()
            })
            .unwrap();
        let mut stored = new_record(
            order,
            Role::Sender,
            Some("owner-network".into()),
            "request-hash".into(),
        );
        delivery.charity_schedule(&mut stored).unwrap();
        assert!(!stored.resume_requested);
        assert_eq!(stored.view.phase, Phase::Prepared);
        stored.confirmed = true;
        assert!(delivery.charity_schedule(&mut stored).is_err());
        stored.authorized_by = Some(LocalIdentity::generate().device_id().unwrap());
        delivery.charity_schedule(&mut stored).unwrap();
        assert!(stored.resume_requested);
        assert_eq!(stored.view.phase, Phase::Transferring);
        let body = delivery
            .with_store(|store| store.charity_order(&stored.view.order_id))
            .unwrap()
            .unwrap();
        let persisted: StoredCharity = serde_json::from_str(&body).unwrap();
        assert!(persisted.confirmed && persisted.resume_requested);
        assert!(!dir.join("delivery-wallet.json").exists());
        assert!(
            delivery
                .charity_get("another-network", &stored.view.order_id)
                .is_err()
        );
        drop(delivery);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
