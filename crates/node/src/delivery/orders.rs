use super::*;

#[derive(Serialize, Deserialize)]
struct SourceDraft {
    request_hash: String,
    request: Prepare,
    unsigned: UnsignedOrder,
    binding_signature: vibeface_delivery::PeerSignature,
    randomness: Vec<WindowRandomness>,
}
#[derive(Deserialize)]
pub(super) struct Announcement {
    pub peer_id: String,
    pub paid_enabled: bool,
    pub profile: ProofProfile,
    pub provider_wallet: String,
    pub asset: String,
    pub chain_id: Word,
    pub escrow: [u8; 20],
    pub delivery_fee: String,
    pub reservation_fee: String,
    pub priority_fee: String,
}
#[derive(Deserialize, Serialize)]
pub(super) struct Quotation {
    pub signature: vibeface_delivery::PeerSignature,
    pub wallet_signature: Vec<u8>,
    pub grants: Vec<ServiceGrant>,
}

impl Delivery {
    pub(super) async fn reconcile(
        &self,
        node: &Runtime,
        mut stored: StoredOrder,
    ) -> Result<StoredOrder> {
        self.ensure_mode(stored.view.mode)?;
        if stored.view.role == Role::Receiver && !stored.accepted {
            return Ok(stored);
        }
        self.authorize_order_effect(&stored).await?;
        match stored.view.role {
            Role::Sender => {
                if matches!(
                    stored.view.phase,
                    Phase::Prepared | Phase::FundingPending | Phase::Reconciling
                ) {
                    if stored.view.mode == Mode::Paid {
                        let fund_id = format!("{}/fund", stored.view.order_id);
                        let funded = self.with_store(|store| store.find_transaction(&fund_id))?;
                        let Some(tx) = funded else {
                            return Ok(stored);
                        };
                        let observed = self.chain().await?.observe(&tx).await?;
                        self.with_store(|store| {
                            store.observe_transaction(
                                &fund_id,
                                &observed.phase,
                                observed.block_number,
                                observed.block_hash.as_deref(),
                                observed.error.as_deref(),
                            )
                        })?;
                        if observed.phase != "confirmed" {
                            stored.view.phase = Phase::Reconciling;
                            stored.view.payment.state = PaymentState::Unknown;
                            self.save(&mut stored)?;
                            return Ok(stored);
                        }
                        stored.view.payment.state = PaymentState::Confirmed;
                    } else if stored.view.phase == Phase::Prepared {
                        return Ok(stored);
                    }
                }
                self.refund_expired(&mut stored).await?;
                self.save(&mut stored)?;
                if matches!(stored.view.phase, Phase::Paid | Phase::Refunded) {
                    return Ok(stored);
                }
                let offer = ServiceRequest::Offer {
                    mode: stored.view.mode,
                    order: stored.order.clone(),
                    file_bytes: stored.view.bytes,
                    intent_id: stored.view.intent_id.clone(),
                    wallet_signature: stored.wallet_signature.clone(),
                };
                service::request(node, &stored.view.provider_peer, &offer).await?;
                for index in 0..stored.order.unsigned.windows.len() {
                    if stored.view.windows[index].state == WindowState::Refunded {
                        continue;
                    }
                    for operation in [GrantOperation::PutCiphertext, GrantOperation::PutCapsule] {
                        self.upload(node, &stored, index as u16, operation).await?;
                    }
                    stored.view.windows[index].committed_bytes =
                        stored.order.unsigned.windows[index].ciphertext_bytes;
                    stored.view.windows[index].state = WindowState::Stored;
                    stored.view.phase = Phase::Transferring;
                    self.save(&mut stored)?;
                }
                if !stored.notified {
                    service::request(node, &stored.view.receiver_peer, &offer).await?;
                    stored.notified = true;
                    self.save(&mut stored)?;
                }
                let snapshot: DeliveryOrder = serde_json::from_value(
                    service::request(
                        node,
                        &stored.view.provider_peer,
                        &ServiceRequest::Order {
                            order_id: stored.view.order_id.clone(),
                        },
                    )
                    .await?,
                )?;
                ensure!(
                    snapshot.order_id == stored.view.order_id,
                    "provider returned another delivery order"
                );
                for (local, remote) in stored.view.windows.iter_mut().zip(snapshot.windows) {
                    local.state = remote.state;
                    local.decryption_proven = remote.decryption_proven;
                }
                if stored.view.mode == Mode::Paid {
                    self.update_paid_view(&mut stored).await?;
                    self.refund_expired(&mut stored).await?;
                } else if snapshot.evidence == Evidence::CharityReceipt {
                    stored.view.evidence = Evidence::CharityReceipt;
                    if stored.view.windows.iter().all(|w| {
                        matches!(
                            w.state,
                            WindowState::Held | WindowState::KeyPublished | WindowState::Earned
                        )
                    }) {
                        stored.view.phase = Phase::Recoverable;
                    }
                }
            }
            Role::Receiver => {
                if !stored.accepted {
                    return Ok(stored);
                }
                for index in 0..stored.order.unsigned.windows.len() {
                    let index = index as u16;
                    let state = self.paid_state(&stored.order).await?;
                    let fact = state
                        .windows
                        .get(index as usize)
                        .context("chain window is missing")?;
                    if fact.phase == 4 && fact.capsule.is_empty() {
                        self.update_paid_view(&mut stored).await?;
                        self.save(&mut stored)?;
                        return Ok(stored);
                    }
                    if !self.with_store(|store| {
                        store.object_complete(&stored.view.order_id, index, "ciphertext")
                    })? {
                        self.download(node, &mut stored, index, GrantOperation::GetCiphertext)
                            .await?;
                    }
                    let mut capsule = fact.capsule.clone();
                    let new_hold = capsule.is_empty();
                    if new_hold {
                        let statement =
                            Statement::window(&stored.order.unsigned, index, Relation::Hold)?;
                        let consent = signature(self.identity.sign_payload(
                            "delivery-accept",
                            &statement.consent_signing_payload()?,
                        )?)?;
                        let ciphertext = self.with_store(|store| {
                            store.object(&stored.view.order_id, index, "ciphertext")
                        })?;
                        let config = self
                            .config()
                            .prover
                            .context("receiving prover is not configured")?;
                        let input = ProofInput {
                            profile: config.profile.clone(),
                            statement,
                            witness: Witness::Hold {
                                order: stored.order.unsigned.clone(),
                                index,
                                receiving_secret: self
                                    .receiver_secret(stored.order.unsigned.terms.binding.key_id)?,
                                ciphertext,
                                consent,
                            },
                        };
                        stored.view.phase = Phase::Proving;
                        self.save(&mut stored)?;
                        let seal = self
                            .prove_cached(&config, &stored, index, "hold", input)
                            .await?;
                        self.authorize_order_effect(&stored).await?;
                        let value = service::request(
                            node,
                            &stored.view.provider_peer,
                            &ServiceRequest::Hold {
                                order_id: stored.view.order_id.clone(),
                                index,
                                consent,
                                seal,
                            },
                        )
                        .await?;
                        let remote: DeliveryOrder = serde_json::from_value(value)?;
                        if remote.phase == Phase::Reconciling {
                            stored.view.phase = Phase::Reconciling;
                            self.save(&mut stored)?;
                            return Ok(stored);
                        }
                        capsule = self
                            .chain()
                            .await?
                            .window(stored.order.unsigned.terms.order_id()?, index)
                            .await?
                            .capsule;
                    }
                    ensure!(
                        capsule.len() == 80
                            && hash(&capsule)
                                == stored.order.unsigned.windows[index as usize].capsule_hash,
                        "confirmed key capsule is unavailable or differs from the manifest"
                    );
                    self.with_store(|store| {
                        store.write_object(&stored.view.order_id, index, "capsule", &capsule)
                    })?;
                    let ciphertext = self.with_store(|store| {
                        store.object(&stored.view.order_id, index, "ciphertext")
                    })?;
                    recover_window(
                        &stored.order.unsigned,
                        index,
                        &self.receiver_secret(stored.order.unsigned.terms.binding.key_id)?,
                        &ciphertext,
                        &capsule,
                    )?;
                    stored.view.windows[index as usize].state = WindowState::KeyPublished;
                    if new_hold {
                        stored.view.phase = Phase::Partial;
                        self.save(&mut stored)?;
                        return Ok(stored);
                    }
                }
                let recovered = self.read(&stored)?;
                stored.view.name = Some(text(&recovered, "name")?.into());
                stored.view.sha256 = Some(text(&recovered, "sha256")?.into());
                stored.view.phase = Phase::Recoverable;
                stored.view.evidence = Evidence::Recoverable;
                self.update_paid_view(&mut stored).await?;
                self.save(&mut stored)?;
                for index in 0..stored.order.unsigned.windows.len() {
                    if stored.view.windows[index].decryption_proven
                        || stored.view.windows[index].state == WindowState::Refunded
                    {
                        continue;
                    }
                    let index = index as u16;
                    let (ciphertext, capsule) = self.with_store(|store| {
                        Ok((
                            store.object(&stored.view.order_id, index, "ciphertext")?,
                            store.object(&stored.view.order_id, index, "capsule")?,
                        ))
                    })?;
                    let config = self
                        .config()
                        .prover
                        .context("receiving prover is not configured")?;
                    let input = ProofInput {
                        profile: config.profile.clone(),
                        statement: Statement::window(
                            &stored.order.unsigned,
                            index,
                            Relation::Open,
                        )?,
                        witness: Witness::Open {
                            order: stored.order.unsigned.clone(),
                            index,
                            receiving_secret: self
                                .receiver_secret(stored.order.unsigned.terms.binding.key_id)?,
                            ciphertext,
                            capsule,
                        },
                    };
                    stored.view.phase = Phase::Proving;
                    self.save(&mut stored)?;
                    let seal = self
                        .prove_cached(&config, &stored, index, "open", input)
                        .await?;
                    self.authorize_order_effect(&stored).await?;
                    service::request(
                        node,
                        &stored.view.provider_peer,
                        &ServiceRequest::Open {
                            order_id: stored.view.order_id.clone(),
                            index,
                            seal,
                        },
                    )
                    .await?;
                    self.update_paid_view(&mut stored).await?;
                    self.save(&mut stored)?;
                    return Ok(stored);
                }
            }
            Role::Provider => {
                if stored.view.mode == Mode::Paid {
                    self.update_paid_view(&mut stored).await?;
                }
            }
        }
        self.save(&mut stored)?;
        Ok(stored)
    }
    async fn upload(
        &self,
        node: &Runtime,
        stored: &StoredOrder,
        index: u16,
        operation: GrantOperation,
    ) -> Result<()> {
        self.authorize_order_effect(stored).await?;
        let grant: ServiceGrant = serde_json::from_value(
            service::request(
                node,
                &stored.view.provider_peer,
                &ServiceRequest::Grant {
                    order_id: stored.view.order_id.clone(),
                    index,
                    operation: operation.clone(),
                },
            )
            .await?,
        )?;
        grant.verify(
            &stored.view.provider_peer,
            &self.identity.device_id()?,
            now(),
        )?;
        let kind = if operation == GrantOperation::PutCapsule {
            "capsule"
        } else {
            "ciphertext"
        };
        let bytes = self.with_store(|store| store.object(&stored.view.order_id, index, kind))?;
        ensure!(
            hex::encode(hash(&bytes)) == grant.body.object_hash
                && bytes.len() == grant.body.max_bytes as usize,
            "provider upload grant changed the object"
        );
        let value = service::request(
            node,
            &stored.view.provider_peer,
            &ServiceRequest::Offset {
                grant: grant.clone(),
            },
        )
        .await?;
        let mut offset = value["offset"].as_u64().context("invalid service offset")? as usize;
        ensure!(
            offset <= bytes.len(),
            "service offset exceeds the complete object"
        );
        while offset < bytes.len() {
            self.authorize_order_effect(stored).await?;
            let end = (offset + CHUNK_BYTES).min(bytes.len());
            let value = service::request(
                node,
                &stored.view.provider_peer,
                &ServiceRequest::Put {
                    grant: grant.clone(),
                    offset: offset as u32,
                    bytes: STANDARD.encode(&bytes[offset..end]),
                },
            )
            .await?;
            ensure!(
                value["offset"].as_u64() == Some(end as u64),
                "service did not confirm the durable prefix"
            );
            offset = end;
        }
        Ok(())
    }
    async fn download(
        &self,
        node: &Runtime,
        stored: &mut StoredOrder,
        index: u16,
        operation: GrantOperation,
    ) -> Result<()> {
        self.authorize_order_effect(stored).await?;
        let grant: ServiceGrant = serde_json::from_value(
            service::request(
                node,
                &stored.view.provider_peer,
                &ServiceRequest::Grant {
                    order_id: stored.view.order_id.clone(),
                    index,
                    operation: operation.clone(),
                },
            )
            .await?,
        )?;
        grant.verify(
            &stored.view.provider_peer,
            &self.identity.device_id()?,
            now(),
        )?;
        let kind = if operation == GrantOperation::GetCapsule {
            "capsule"
        } else {
            "ciphertext"
        };
        let spec = stored
            .order
            .unsigned
            .windows
            .get(index as usize)
            .context("invalid delivery window")?;
        ensure!(
            grant.body.object_hash
                == hex::encode(if kind == "capsule" {
                    spec.capsule_hash
                } else {
                    spec.ciphertext_hash
                })
                && grant.body.max_bytes
                    == if kind == "capsule" {
                        80
                    } else {
                        spec.ciphertext_bytes
                    },
            "provider read grant changed the object"
        );
        let mut offset = self.with_store(|store| {
            store.reserve_object(
                &stored.view.order_id,
                index,
                kind,
                &grant.body.object_hash,
                grant.body.max_bytes,
            )
        })?;
        while offset < grant.body.max_bytes {
            self.authorize_order_effect(stored).await?;
            let value = service::request(
                node,
                &stored.view.provider_peer,
                &ServiceRequest::Get {
                    grant: grant.clone(),
                    offset,
                },
            )
            .await?;
            let encoded = text(&value, "bytes")?;
            ensure!(
                encoded.len() <= CHUNK_BYTES.div_ceil(3) * 4,
                "service chunk exceeds limit"
            );
            let bytes = STANDARD.decode(encoded)?;
            let next = self.with_store(|store| {
                store.append_object(&stored.view.order_id, index, kind, offset, &bytes)
            })?;
            ensure!(
                value["next_offset"].as_u64() == Some(u64::from(next)),
                "service chunk offset mismatch"
            );
            offset = next;
            if kind == "ciphertext" {
                stored.view.windows[index as usize].committed_bytes = offset;
                self.save(stored)?;
            }
        }
        Ok(())
    }
    async fn prove_cached(
        &self,
        config: &proof::ProofConfig,
        stored: &StoredOrder,
        index: u16,
        _relation: &str,
        input: ProofInput,
    ) -> Result<Vec<u8>> {
        let digest = hex::encode(input.statement.digest());
        let id = format!("proof-seal:{digest}");
        if let Some(seal) = self.with_store(|store| store.value(&id))? {
            return Ok(seal);
        }
        ensure!(
            input.statement
                == Statement::window(&stored.order.unsigned, index, input.statement.relation()?)?,
            "proof job differs from the authorized window"
        );
        let receipt = self.with_store(|store| store.reserve_proof(&digest))?;
        let seal = self.proofs.prove(config, &input, &receipt).await?;
        self.with_store(|store| store.put_value(&id, &seal))?;
        Ok(seal)
    }
    async fn authorize_order_effect(&self, order: &StoredOrder) -> Result<()> {
        self.ensure_mode(order.view.mode)?;
        if order.view.role == Role::Provider && order.network_id.is_none() {
            return Ok(());
        }
        self.authorize_effect(
            order
                .network_id
                .as_deref()
                .context("delivery order has no personal network")?,
            order
                .authorized_by
                .as_deref()
                .context("delivery order has no managing member authorization")?,
        )
        .await
    }
    pub(super) fn read(&self, stored: &StoredOrder) -> Result<Value> {
        ensure!(
            stored.view.role == Role::Receiver && stored.accepted,
            "only an accepted receiving Node can read this file"
        );
        let secret = self.receiver_secret(stored.order.unsigned.terms.binding.key_id)?;
        let mut bytes = Zeroizing::new(Vec::new());
        for window in &stored.order.unsigned.windows {
            let (ciphertext, capsule) = self.with_store(|store| {
                Ok((
                    store.object(&stored.view.order_id, window.index, "ciphertext")?,
                    store.object(&stored.view.order_id, window.index, "capsule")?,
                ))
            })?;
            let recovered = recover_window(
                &stored.order.unsigned,
                window.index,
                &secret,
                &ciphertext,
                &capsule,
            )?;
            bytes.extend_from_slice(&recovered.plaintext);
        }
        let (name, content) = open_envelope(&bytes)?;
        ensure!(
            content.len() as u64 == stored.view.bytes,
            "recovered original file length differs from its order"
        );
        let digest = hex::encode(hash(content));
        ensure!(
            stored
                .view
                .sha256
                .as_ref()
                .is_none_or(|expected| expected == &digest),
            "recovered original file digest changed"
        );
        Ok(
            json!({"name":name,"content_base64":STANDARD.encode(content),"sha256":digest,"bytes":content.len()}),
        )
    }
    async fn refund_expired(&self, stored: &mut StoredOrder) -> Result<()> {
        let chain = self.chain().await?;
        let state = self.paid_state(&stored.order).await?;
        let terms = &stored.order.unsigned.terms;
        let at = chain.confirmed_position().await?.timestamp;
        let key = chain.current_key(terms.binding.receiver_peer).await?;
        for (index, window) in state.windows.iter().enumerate() {
            let refundable = match window.phase {
                0 => at > terms.hold_deadline || !key.active || key.serial != terms.binding.serial,
                1 => at > terms.key_deadline,
                2 => {
                    terms.acceptance == vibeface_delivery::Acceptance::Strict
                        && at > terms.open_deadline
                }
                _ => false,
            };
            if refundable {
                chain
                    .execute(
                        self,
                        &format!("{}/refund-{index}", stored.view.order_id),
                        chain::Call::Refund {
                            order_id: terms.order_id()?,
                            index: index as u16,
                        },
                        &stored.view.payment.gas_cap,
                    )
                    .await?;
            }
        }
        if !state.bonus_earned
            && !state.bonus_refunded
            && (at > terms.priority_deadline || state.windows.iter().any(|w| w.phase == 4))
        {
            chain
                .execute(
                    self,
                    &format!("{}/refund-bonus", stored.view.order_id),
                    chain::Call::RefundBonus {
                        order_id: terms.order_id()?,
                    },
                    &stored.view.payment.gas_cap,
                )
                .await?;
        }
        self.update_paid_view(stored).await
    }
    pub(super) async fn prepare(
        &self,
        node: &Runtime,
        network: &str,
        request: Prepare,
    ) -> Result<StoredOrder> {
        self.ensure_mode(request.mode)?;
        ensure!(
            request.mode == Mode::Paid,
            "charity uses its independent signed protocol"
        );
        self.config()
            .prover
            .context("sending Node requires a trusted local prover")?
            .readiness()?;
        ensure!(
            uuid::Uuid::parse_str(&request.intent_id)?.to_string() == request.intent_id,
            "intent_id must be a canonical UUID"
        );
        parse_word(&request.receiver_peer)?;
        parse_word(&request.provider_peer)?;
        file_name(&request.name)?;
        ensure!(
            request.content_base64.len() <= MAX_FILE_BYTES.div_ceil(3) * 4,
            "file exceeds profile limit"
        );
        let file = Zeroizing::new(STANDARD.decode(&request.content_base64)?);
        let bytes = Zeroizing::new(envelope(&request.name, &file)?);
        let request_hash = hex::encode(hash(&serde_json::to_vec(&request)?));
        ensure!(
            self.with_store(|store| store.charity_by_intent(network, &request.intent_id))?
                .is_none(),
            "delivery intent was already used for a charity order"
        );
        if let Some(prior) =
            self.with_store(|store| store.by_intent(network, &request.intent_id))?
        {
            ensure!(
                prior.request_hash == request_hash,
                "delivery intent was already used for different content or costs"
            );
            return Ok(prior);
        }
        let source = self.identity.device_id()?;
        ensure!(
            source != request.receiver_peer
                && source != request.provider_peer
                && request.receiver_peer != request.provider_peer,
            "this experimental route requires distinct sender, receiver and service Nodes"
        );
        let draft_id = format!("source-draft:{network}:{}", request.intent_id);
        let draft = if let Some(encoded) = self.with_store(|store| store.value(&draft_id))? {
            let encoded = Zeroizing::new(encoded);
            let draft: SourceDraft = serde_json::from_slice(&encoded)?;
            ensure!(
                draft.request_hash == request_hash,
                "delivery intent was already used for different content or costs"
            );
            draft
        } else {
            let announcement: Announcement = serde_json::from_value(
                service::request(node, &request.provider_peer, &ServiceRequest::Announce).await?,
            )?;
            ensure!(
                announcement.peer_id == request.provider_peer,
                "service announcement belongs to another Node"
            );
            ensure!(
                announcement.paid_enabled,
                "service does not support paid delivery"
            );
            let binding: SignedKeyBinding = serde_json::from_value(
                service::request(
                    node,
                    &request.receiver_peer,
                    &ServiceRequest::KeyBinding { mode: request.mode },
                )
                .await?,
            )?;
            binding.verify()?;
            ensure!(
                binding.binding.receiver_peer == parse_word(&request.receiver_peer)?,
                "receiving key belongs to another Node"
            );
            let config = self.config();
            let profile = config
                .chain
                .as_ref()
                .context("paid deployment missing")?
                .profile
                .clone();
            let (chain_id, escrow) = deployment(&config, request.mode)?;
            ensure!(
                binding.binding.chain_id == chain_id && binding.binding.escrow == escrow,
                "receiving key belongs to another deployment"
            );
            if request.mode == Mode::Paid {
                ensure!(
                    announcement.chain_id == chain_id
                        && announcement.escrow == escrow
                        && announcement.profile == profile,
                    "service uses a different pinned proof deployment"
                );
                let current = self
                    .chain()
                    .await?
                    .current_key(binding.binding.receiver_peer)
                    .await?;
                ensure!(
                    current.active
                        && current.serial == binding.binding.serial
                        && current.binding_hash == binding.binding.digest()?,
                    "receiving key is not registered on the local escrow"
                );
            }
            let windows = bytes.len().div_ceil(65536).max(1) as u16;
            let (delivery_fee, reservation_fee, bonus) = (
                Amount::from_decimal_str(&announcement.delivery_fee)?,
                Amount::from_decimal_str(&announcement.reservation_fee)?,
                Amount::from_decimal_str(&announcement.priority_fee)?,
            );
            let mut total = bonus;
            for _ in 0..windows {
                total = total
                    .checked_add(delivery_fee)?
                    .checked_add(reservation_fee)?;
            }
            ensure!(
                total.to_be_bytes()
                    <= Amount::from_decimal_str(&request.max_service_amount)?.to_be_bytes(),
                "service quotation exceeds your authorized amount"
            );
            Amount::from_decimal_str(&request.max_gas_amount)?;
            let payer = self
                .wallet
                .lock()
                .expect("delivery wallet")
                .address()?
                .as_slice()
                .try_into()
                .expect("wallet address");
            let provider_wallet = address_bytes(&announcement.provider_wallet)?;
            let asset = config
                .chain
                .as_ref()
                .context("paid deployment missing")?
                .asset
                .clone();
            ensure!(
                asset == announcement.asset,
                "service asset differs from pinned deployment"
            );
            let asset = address_bytes(&asset)?;
            let at = now();
            let deadlines = paid_deadlines(at, &profile);
            let terms = Terms {
                profile_id: profile.digest()?,
                guest_image_id: profile.guest_image_id,
                binding: binding.binding,
                source_peer: parse_word(&source)?,
                provider_peer: parse_word(&request.provider_peer)?,
                payer,
                provider_wallet,
                asset,
                acceptance: match request.acceptance {
                    Acceptance::Recoverable => vibeface_delivery::Acceptance::Recoverable,
                    Acceptance::Strict => vibeface_delivery::Acceptance::Strict,
                },
                total_plaintext_bytes: bytes.len() as u64,
                window_count: windows,
                whole_order_bonus: bonus,
                escrow_amount: total,
                quote_expires_at: deadlines.quote_expires_at,
                hold_deadline: deadlines.hold_deadline,
                key_deadline: deadlines.key_deadline,
                open_deadline: deadlines.open_deadline,
                retain_until: deadlines.retain_until,
                priority_deadline: deadlines.priority_deadline,
                payer_order_nonce: random_word(),
            };
            terms.validate()?;
            let mut encrypted = Vec::new();
            let mut randomness = Vec::new();
            for (index, window) in bytes.chunks(65536).enumerate() {
                let random = WindowRandomness {
                    salt: Secret32::new(random_word())?,
                    data_key: Secret32::new(random_word())?,
                    nonce: rand::random(),
                    hpke_seed: Secret32::new(random_word())?,
                };
                encrypted.push(encrypt_window(
                    &terms,
                    index as u16,
                    (index * 65536) as u64,
                    window,
                    &random,
                    delivery_fee,
                    reservation_fee,
                )?);
                randomness.push(random);
            }
            let unsigned = UnsignedOrder {
                terms,
                windows: encrypted.iter().map(|w| w.spec.clone()).collect(),
                quote_nonce: random_word(),
            };
            unsigned.validate()?;
            profile.check_order(&unsigned)?;
            let order_id = hex::encode(unsigned.terms.order_id()?);
            let draft = SourceDraft {
                request_hash: request_hash.clone(),
                request,
                unsigned,
                binding_signature: binding.signature,
                randomness,
            };
            let encoded = Zeroizing::new(serde_json::to_vec(&draft)?);
            self.with_store(|store| {
                store.put_value(&draft_id, &encoded)?;
                for window in &encrypted {
                    store.write_object(
                        &order_id,
                        window.spec.index,
                        "ciphertext",
                        &window.ciphertext,
                    )?;
                    store.write_object(&order_id, window.spec.index, "capsule", &window.capsule)?;
                }
                Ok(())
            })?;
            draft
        };
        let SourceDraft {
            request,
            unsigned,
            binding_signature,
            randomness,
            ..
        } = draft;
        for (index, random) in randomness.iter().enumerate() {
            let window = &unsigned.windows[index];
            let start = window.offset as usize;
            let end = start + window.plaintext_bytes as usize;
            let encrypted = encrypt_window(
                &unsigned.terms,
                index as u16,
                window.offset,
                &bytes[start..end],
                random,
                window.delivery_fee,
                window.reservation_fee,
            )?;
            ensure!(
                encrypted.spec == *window,
                "persisted source draft does not match its original encrypted manifest"
            );
            self.with_store(|store| {
                let id = hex::encode(unsigned.terms.order_id()?);
                store.write_object(&id, index as u16, "ciphertext", &encrypted.ciphertext)?;
                store.write_object(&id, index as u16, "capsule", &encrypted.capsule)
            })?;
        }
        let quotation: Quotation = serde_json::from_value(
            service::request(
                node,
                &request.provider_peer,
                &ServiceRequest::Quote {
                    mode: request.mode,
                    order: unsigned.clone(),
                    binding: SignedKeyBinding {
                        binding: unsigned.terms.binding.clone(),
                        signature: binding_signature,
                    },
                    file_bytes: file.len() as u64,
                    intent_id: request.intent_id.clone(),
                },
            )
            .await?,
        )?;
        let source_signature = signature(
            self.identity
                .sign_payload("delivery-manifest", &unsigned.source_signing_payload()?)?,
        )?;
        let order = Order::signed(
            unsigned,
            binding_signature,
            quotation.signature,
            source_signature,
        )?;
        let profile = self
            .config()
            .chain
            .context("paid deployment missing")?
            .profile;
        let mut stored = StoredOrder {
            network_id: Some(network.into()),
            authorized_by: None,
            confirmed: false,
            resume_requested: false,
            request_hash,
            view: view(
                &request.intent_id,
                Role::Sender,
                request.mode,
                &order,
                file.len() as u64,
            )?,
            order,
            wallet_signature: quotation.wallet_signature,
            source_seals: Vec::new(),
            grants: quotation.grants,
            accepted: false,
            notified: false,
        };
        stored.view.name = Some(request.name);
        stored.view.sha256 = Some(hex::encode(hash(&file)));
        stored.view.payment.gas_cap = if stored.view.mode == Mode::Paid {
            request.max_gas_amount
        } else {
            "0".into()
        };
        self.with_store(|store| {
            for (index, randomness) in randomness.into_iter().enumerate() {
                let statement =
                    Statement::window(&stored.order.unsigned, index as u16, Relation::Source)?;
                let ciphertext = store.object(&stored.view.order_id, index as u16, "ciphertext")?;
                let capsule = store.object(&stored.view.order_id, index as u16, "capsule")?;
                let start = index * 65536;
                let end = (start + 65536).min(bytes.len());
                let input = ProofInput {
                    profile: profile.clone(),
                    statement,
                    witness: Witness::Source {
                        order: Box::new(stored.order.clone()),
                        index: index as u16,
                        plaintext: SecretBytes::new(bytes[start..end].to_vec()),
                        randomness,
                        ciphertext,
                        capsule,
                    },
                };
                let encoded = Zeroizing::new(serde_json::to_vec(&input)?);
                store.put_value(
                    &format!("source-input:{}:{index}", stored.view.order_id),
                    &encoded,
                )?;
            }
            store.save_order(&stored)
        })?;
        Ok(stored)
    }
    pub(super) async fn confirm(
        &self,
        node: &Runtime,
        mut order: StoredOrder,
    ) -> Result<StoredOrder> {
        self.ensure_mode(order.view.mode)?;
        self.authorize_order_effect(&order).await?;
        ensure!(
            order.view.role == Role::Sender,
            "only a sending Node confirms its prepared intent"
        );
        if !matches!(
            order.view.phase,
            Phase::Prepared | Phase::Proving | Phase::FundingPending | Phase::Reconciling
        ) {
            return self.reconcile(node, order).await;
        }
        if order.view.mode == Mode::Paid {
            let fund_id = format!("{}/fund", order.view.order_id);
            if let Some(transaction) = self.with_store(|store| store.find_transaction(&fund_id))? {
                let observed = self.chain().await?.observe(&transaction).await?;
                self.with_store(|store| {
                    store.observe_transaction(
                        &fund_id,
                        &observed.phase,
                        observed.block_number,
                        observed.block_hash.as_deref(),
                        observed.error.as_deref(),
                    )
                })?;
                if observed.phase == "confirmed" {
                    order.view.phase = Phase::Funded;
                    order.view.payment.state = PaymentState::Confirmed;
                    order.view.payment.tx_hash = transaction.tx_hash;
                    self.save(&mut order)?;
                    return self.reconcile(node, order).await;
                }
                if matches!(
                    observed.phase.as_str(),
                    "reverted" | "conflicted" | "failed"
                ) {
                    order.view.phase = Phase::Failed;
                    order.view.payment.state = PaymentState::Failed;
                    order.view.error = observed.error;
                    self.save(&mut order)?;
                    return Ok(order);
                }
            }
            ensure!(
                self.chain().await?.confirmed_position().await?.timestamp
                    <= order.order.unsigned.terms.quote_expires_at,
                "quotation expired; this intent will not be paid"
            );
            let config = self.config();
            let prover = config.prover.context("paid prover is not configured")?;
            if order.source_seals.len() < order.order.unsigned.windows.len() {
                order.view.phase = Phase::Proving;
                self.save(&mut order)?;
                let index = order.source_seals.len();
                let encoded = Zeroizing::new(
                    self.with_store(|store| {
                        store.value(&format!("source-input:{}:{index}", order.view.order_id))
                    })?
                    .context("source proof witness unavailable")?,
                );
                let input: ProofInput = serde_json::from_slice(&encoded)?;
                let seal = self
                    .prove_cached(&prover, &order, index as u16, "source", input)
                    .await?;
                order.source_seals.push(seal);
                self.save(&mut order)?;
                return Ok(order);
            }
            order.view.phase = Phase::FundingPending;
            self.save(&mut order)?;
            let chain = self.chain().await?;
            let approve = chain
                .execute(
                    self,
                    &format!("{}/approve", order.view.order_id),
                    chain::Call::Approve {
                        amount: order.order.unsigned.terms.escrow_amount,
                    },
                    &order.view.payment.gas_cap,
                )
                .await?;
            if approve.phase != "confirmed" {
                order.view.phase = Phase::Reconciling;
                order.view.payment.state = PaymentState::Unknown;
                self.save(&mut order)?;
                return Ok(order);
            }
            let fund = chain
                .execute(
                    self,
                    &format!("{}/fund", order.view.order_id),
                    chain::Call::Fund {
                        order: Box::new(order.order.clone()),
                        source_seals: order.source_seals.clone(),
                        wallet_signature: order.wallet_signature.clone(),
                    },
                    &order.view.payment.gas_cap,
                )
                .await?;
            let tx = self
                .with_store(|store| store.transaction(&format!("{}/fund", order.view.order_id)))?;
            order.view.payment.tx_hash = tx.tx_hash;
            if fund.phase != "confirmed" {
                order.view.phase = Phase::Reconciling;
                order.view.payment.state = PaymentState::Unknown;
                self.save(&mut order)?;
                return Ok(order);
            }
            order.view.payment.state = PaymentState::Confirmed;
        }
        order.view.phase = Phase::Funded;
        self.save(&mut order)?;
        self.reconcile(node, order).await
    }
}

pub(super) fn address_bytes(value: &str) -> Result<[u8; 20]> {
    ensure!(
        value.len() == 42 && value.starts_with("0x"),
        "invalid EVM address"
    );
    hex::decode(&value[2..])?
        .try_into()
        .ok()
        .context("invalid EVM address length")
}
pub(super) fn view(
    intent: &str,
    role: Role,
    mode: Mode,
    order: &Order,
    file_bytes: u64,
) -> Result<DeliveryOrder> {
    let terms = &order.unsigned.terms;
    let mut payment = Payment::free();
    if mode == Mode::Paid {
        let mut delivery = Amount::ZERO;
        let mut reservation = Amount::ZERO;
        for window in &order.unsigned.windows {
            delivery = delivery.checked_add(window.delivery_fee)?;
            reservation = reservation.checked_add(window.reservation_fee)?;
        }
        payment = Payment {
            state: PaymentState::Prepared,
            chain_id: Some(
                u64::from_be_bytes(terms.binding.chain_id[24..].try_into().expect("chain"))
                    .to_string(),
            ),
            asset: Some(format!("0x{}", hex::encode(terms.asset))),
            payer: Some(format!("0x{}", hex::encode(terms.payer))),
            provider_wallet: Some(format!("0x{}", hex::encode(terms.provider_wallet))),
            delivery_fee: delivery.to_decimal_string(),
            reservation_fee: reservation.to_decimal_string(),
            priority_fee: terms.whole_order_bonus.to_decimal_string(),
            total: terms.escrow_amount.to_decimal_string(),
            gas_cap: "0".into(),
            earned: "0".into(),
            withdrawn: "0".into(),
            refunded: "0".into(),
            tx_hash: None,
        };
    }
    Ok(DeliveryOrder {
        order_id: hex::encode(terms.order_id()?),
        intent_id: intent.into(),
        role,
        mode,
        acceptance: match terms.acceptance {
            vibeface_delivery::Acceptance::Recoverable => Acceptance::Recoverable,
            vibeface_delivery::Acceptance::Strict => Acceptance::Strict,
        },
        source_peer: hex::encode(terms.source_peer),
        receiver_peer: hex::encode(terms.binding.receiver_peer),
        provider_peer: hex::encode(terms.provider_peer),
        name: None,
        bytes: file_bytes,
        sha256: None,
        phase: Phase::Prepared,
        evidence: Evidence::None,
        payment,
        deadlines: Some(Deadlines {
            quote_expires_at: terms.quote_expires_at,
            hold_deadline: terms.hold_deadline,
            key_deadline: terms.key_deadline,
            open_deadline: terms.open_deadline,
            retain_until: terms.retain_until,
            priority_deadline: terms.priority_deadline,
        }),
        windows: order
            .unsigned
            .windows
            .iter()
            .map(|window| WindowView {
                index: window.index,
                bytes: window.plaintext_bytes,
                committed_bytes: 0,
                state: WindowState::Pending,
                decryption_proven: false,
            })
            .collect(),
        created_at_ms: crate::runtime::now_ms(),
        updated_at_ms: crate::runtime::now_ms(),
        error: None,
    })
}
