use super::*;

#[derive(Serialize, Deserialize)]
struct SourceDraft {
    request_hash: String,
    request: Prepare,
    unsigned: UnsignedOrder,
    binding_signature: vibeface_delivery::PeerSignature,
    randomness: Vec<WindowRandomness>,
}

#[derive(Debug, PartialEq, Eq)]
enum AllocationRecovery {
    Unallocated,
    Pending,
    Allocated,
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
        self.ensure_mode(Mode::Paid)?;
        ensure!(
            stored.view.mode == Mode::Paid,
            "charity uses its independent signed protocol"
        );
        if stored.view.role == Role::Receiver && stored.accepted {
            self.receiver_secret(stored.order.unsigned.terms.binding.key_id)?;
        }
        if stored.view.role == Role::Receiver && !stored.accepted {
            return Ok(stored);
        }
        if stored.view.mode == Mode::Paid {
            if let Err(error) = self.update_paid_view(&mut stored).await {
                stored.view.chain_unknown();
                self.save(&mut stored)?;
                return Err(error);
            }
            self.save(&mut stored)?;
        }
        self.authorize_order_effect(&stored).await?;
        match stored.view.role {
            Role::Sender => {
                let refunding = self.refund_expired(&mut stored).await?;
                self.save(&mut stored)?;
                if refunding || matches!(stored.view.phase, Phase::Paid | Phase::Refunded) {
                    return Ok(stored);
                }
                let offer = ServiceRequest::Offer {
                    mode: stored.view.mode,
                    order: stored.order.clone(),
                    file_bytes: stored.view.bytes,
                    intent_id: stored.view.intent_id.clone(),
                    wallet_signature: stored.wallet_signature.clone(),
                    budget_id: stored
                        .view
                        .payment
                        .budget_id
                        .clone()
                        .context("protected budget missing")?,
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
                self.update_paid_view(&mut stored).await?;
                self.refund_expired(&mut stored).await?;
            }
            Role::Receiver => {
                if !stored.accepted {
                    return Ok(stored);
                }
                let recovered_locally = stored.view.name.is_some() && stored.view.sha256.is_some();
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
                        let seal = self.prove_cached(&config, &stored, index, input).await?;
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
                            stored.view.chain_unknown();
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
                    if capsule.is_empty() {
                        stored.view.phase = Phase::CipherHeld;
                        self.save(&mut stored)?;
                        continue;
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
                    if !recovered_locally {
                        recover_window(
                            &stored.order.unsigned,
                            index,
                            &self.receiver_secret(stored.order.unsigned.terms.binding.key_id)?,
                            &ciphertext,
                            &capsule,
                        )?;
                    }
                    stored.view.windows[index as usize].state = WindowState::KeyPublished;
                    if new_hold {
                        stored.view.phase = Phase::Partial;
                        self.save(&mut stored)?;
                        return Ok(stored);
                    }
                }
                let mut waiting = false;
                for window in &stored.order.unsigned.windows {
                    waiting |= !self.with_store(|store| {
                        store.object_complete(&stored.view.order_id, window.index, "capsule")
                    })?;
                }
                if waiting {
                    self.update_paid_view(&mut stored).await?;
                    self.save(&mut stored)?;
                    return Ok(stored);
                }
                let recovered = self.read(&stored)?;
                stored.view.name = Some(text(&recovered, "name")?.into());
                stored.view.sha256 = Some(text(&recovered, "sha256")?.into());
                stored.view.phase = Phase::Recoverable;
                stored.view.evidence = Evidence::Recoverable;
                self.update_paid_view(&mut stored).await?;
                self.save(&mut stored)?;
            }
            Role::Provider => self.update_paid_view(&mut stored).await?,
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
        let seal = self.proofs.prove(config, &input, &receipt, None).await?;
        self.with_store(|store| store.put_value(&id, &seal))?;
        Ok(seal)
    }
    pub(super) async fn authorize_order_effect(&self, order: &StoredOrder) -> Result<()> {
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
    async fn refund_expired(&self, stored: &mut StoredOrder) -> Result<bool> {
        let id = stored
            .view
            .payment
            .budget_id
            .clone()
            .context("protected budget missing")?;
        let snapshot = self.budget_snapshot(&id).await?;
        if snapshot.state != "closing" || snapshot.position.timestamp <= snapshot.max_claim_until {
            return Ok(false);
        }
        if snapshot.locked == "0" && snapshot.free == "0" {
            return Ok(false);
        }
        let record = self.budget_record(&id)?;
        let windows = stored
            .order
            .unsigned
            .windows
            .iter()
            .map(|w| budget::WindowRef {
                order_id: stored.view.order_id.clone(),
                index: w.index,
            })
            .collect();
        self.chain()
            .await?
            .execute(
                self,
                &format!("budget-{id}/refund-{}", stored.view.order_id),
                chain::Call::Budget(Box::new(budget::BudgetAction::Refund {
                    budget_id: parse_word(&id)?,
                    windows,
                })),
                &record.payer_gas_cap,
            )
            .await?;
        self.update_paid_view(stored).await?;
        Ok(true)
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
        ensure!(
            request.acceptance == Acceptance::Recoverable,
            "protected delivery supports Recoverable only; Strict is not silently downgraded"
        );
        let budget_id = request
            .budget_id
            .as_deref()
            .context("select a prepaid protected budget before preparing delivery")?;
        parse_word(budget_id)?;
        let snapshot = self.budget_snapshot(budget_id).await?;
        let mut budget_record = self.budget_record(budget_id)?;
        ensure!(
            snapshot.terms == budget_record.terms
                && snapshot.state == "open"
                && snapshot.terms.provider_peer == request.provider_peer
                && snapshot.terms.payer
                    == chain::address_hex(
                        self.wallet
                            .lock()
                            .map_err(|_| anyhow::anyhow!("delivery wallet lock failed"))?
                            .address()?
                    )
                && budget_record.payer_gas_cap == request.max_gas_amount,
            "protected budget roles, open state or cumulative payer gas authorization do not match"
        );
        ensure!(
            budget_record
                .network_id
                .as_deref()
                .is_none_or(|id| id == network),
            "protected budget belongs to another personal network"
        );
        budget_record.network_id = Some(network.into());
        self.with_store(|store| {
            store.put_value(
                &format!("protected-budget:{budget_id}"),
                &serde_json::to_vec(&budget_record)?,
            )
        })?;
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
            ensure!(
                chain::address(&announcement.provider_wallet)?
                    == chain::address(&snapshot.terms.payee)?
                    && announcement.asset == snapshot.terms.asset,
                "protected budget fixes another provider wallet or asset"
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
                    budget_id: request
                        .budget_id
                        .clone()
                        .context("protected budget missing")?,
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
            view: view(&request.intent_id, Role::Sender, &order, file.len() as u64)?,
            order,
            wallet_signature: quotation.wallet_signature,
            source_seals: Vec::new(),
            grants: quotation.grants,
            accepted: false,
            notified: false,
            allocation_batch: Vec::new(),
            chain_observed_at_ms: 0,
            settlement_complete: false,
        };
        stored.view.payment.budget_id = request.budget_id;
        stored.view.name = Some(request.name);
        stored.view.sha256 = Some(hex::encode(hash(&file)));
        stored.view.payment.gas_cap = request.max_gas_amount;
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
    async fn recover_allocation(
        &self,
        chain: &chain::Chain,
        order: &mut StoredOrder,
    ) -> Result<AllocationRecovery> {
        let budget_id = order
            .view
            .payment
            .budget_id
            .as_deref()
            .context("protected budget missing")?;
        let ids = if order.allocation_batch.is_empty() {
            vec![order.view.order_id.clone()]
        } else {
            order.allocation_batch.clone()
        };
        let transaction_id = format!(
            "budget-{budget_id}/reserve-{}",
            hex::encode(hash(&serde_json::to_vec(&ids)?))
        );
        let transaction = self.with_store(|store| store.find_transaction(&transaction_id))?;
        let position = chain.confirmed_position().await?;
        let expected_budget = parse_word(budget_id)?;
        let mut batch = Vec::new();
        let mut allocated = 0;
        for id in &ids {
            let pending = self.with_store(|store| store.order(id))?;
            ensure!(
                pending.confirmed
                    && pending.view.role == Role::Sender
                    && pending.allocation_batch == order.allocation_batch
                    && pending.view.payment.budget_id.as_deref() == Some(budget_id),
                "confirmed source batch changed"
            );
            let actual = chain.order_budget_at(&position, parse_word(id)?).await?;
            ensure!(
                actual == [0; 32] || actual == expected_budget,
                "order allocation belongs to another budget"
            );
            allocated += usize::from(actual == expected_budget);
            batch.push(pending);
        }
        ensure!(
            allocated == 0 || allocated == batch.len(),
            "original allocation batch is only partially observed; reconcile before spending"
        );
        if allocated == batch.len() {
            for pending in &mut batch {
                let (state, budget) = self.paid_state_at(chain, &position, &pending.order).await?;
                ensure!(
                    budget.terms.budget_id == budget_id,
                    "original allocation budget changed"
                );
                provider::apply_paid_snapshot(&mut pending.view, &state)?;
                pending.view.payment.tx_hash = transaction
                    .as_ref()
                    .and_then(|tx| tx.tx_hash.clone())
                    .or_else(|| pending.view.payment.tx_hash.clone());
                pending.view.error = None;
                pending.view.updated_at_ms = crate::runtime::now_ms();
            }
            chain.verify_position(&position).await?;
            self.with_store(|store| store.save_authorized_batch(&batch))?;
            *order = batch
                .into_iter()
                .find(|item| item.view.order_id == order.view.order_id)
                .context("original order missing from allocation batch")?;
            return Ok(AllocationRecovery::Allocated);
        }
        chain.verify_position(&position).await?;
        if let Some(transaction) = transaction.filter(|tx| tx.raw_tx.is_some()) {
            let observed = chain.observe(&transaction).await;
            if !observed.as_ref().is_ok_and(|value| {
                matches!(value.phase.as_str(), "reverted" | "conflicted" | "failed")
            }) {
                order.view.payment.tx_hash = transaction.tx_hash;
                order.view.chain_unknown();
                self.save(order)?;
                return Ok(AllocationRecovery::Pending);
            }
        }
        ensure!(
            batch
                .iter()
                .all(|pending| position.timestamp <= pending.order.unsigned.terms.quote_expires_at),
            "quotation expired; no new allocation will be submitted"
        );
        Ok(AllocationRecovery::Unallocated)
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
            let chain = self.chain().await?;
            match self.recover_allocation(&chain, &mut order).await? {
                AllocationRecovery::Allocated => return self.reconcile(node, order).await,
                AllocationRecovery::Pending => return Ok(order),
                AllocationRecovery::Unallocated => {}
            }
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
                    .prove_cached(&prover, &order, index as u16, input)
                    .await?;
                order.source_seals.push(seal);
                self.save(&mut order)?;
                return Ok(order);
            }
            order.view.phase = Phase::FundingPending;
            self.save(&mut order)?;
            let budget_id = order
                .view
                .payment
                .budget_id
                .clone()
                .context("protected budget missing")?;
            let ids = if order.allocation_batch.is_empty() {
                vec![order.view.order_id.clone()]
            } else {
                order.allocation_batch.clone()
            };
            let mut batch = Vec::new();
            for id in &ids {
                let pending = self.with_store(|store| store.order(id))?;
                ensure!(
                    pending.confirmed
                        && pending.allocation_batch == order.allocation_batch
                        && pending.view.payment.budget_id.as_deref() == Some(&budget_id),
                    "confirmed source batch changed"
                );
                if pending.source_seals.len() != pending.order.unsigned.windows.len() {
                    return Ok(order);
                }
                batch.push(pending);
            }
            let intent = hex::encode(hash(&serde_json::to_vec(&ids)?));
            let allocation = self.reserve_orders(&budget_id, &batch, &intent).await?;
            order.view.payment.tx_hash = allocation.tx_hash.clone();
            if allocation.phase != "confirmed" {
                order.view.chain_unknown();
                self.save(&mut order)?;
                return Ok(order);
            }
            for mut allocated in batch {
                allocated.view.payment.state = PaymentState::Confirmed;
                allocated.view.payment.tx_hash = allocation.tx_hash.clone();
                allocated.view.phase = Phase::Funded;
                self.save(&mut allocated)?;
                if allocated.view.order_id == order.view.order_id {
                    order = allocated;
                }
            }
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
    order: &Order,
    file_bytes: u64,
) -> Result<DeliveryOrder> {
    let terms = &order.unsigned.terms;
    let payment = {
        let mut delivery = Amount::ZERO;
        let mut reservation = Amount::ZERO;
        for window in &order.unsigned.windows {
            delivery = delivery.checked_add(window.delivery_fee)?;
            reservation = reservation.checked_add(window.reservation_fee)?;
        }
        Payment {
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
            budget_id: None,
            settlement: None,
        }
    };
    Ok(DeliveryOrder {
        order_id: hex::encode(terms.order_id()?),
        intent_id: intent.into(),
        role,
        mode: Mode::Paid,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::{SolCall, SolValue};
    use std::collections::BTreeMap;

    #[allow(dead_code)]
    mod support {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/delivery/examples/support/mod.rs"
        ));
    }

    #[test]
    fn recoverable_receiver_reads_original_bytes_without_open_or_chain_and_reports_a_missing_key()
    -> Result<()> {
        let content = b"original recovered bytes";
        let wrapped = envelope("restored.bin", content)?;
        let middle = wrapped.len() / 2;
        let original = support::fixture_with_profile(
            &[middle, wrapped.len() - middle],
            chain::tests::config().profile,
        );
        let fixture = support::fixture_from_terms(
            original.profile,
            original.order.unsigned.terms,
            vec![
                SecretBytes::new(wrapped[..middle].to_vec()),
                SecretBytes::new(wrapped[middle..].to_vec()),
            ],
            Amount::from_u64(2),
            Amount::from_u64(1),
        );
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("receiver-recovery-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let delivery = Delivery::open(&dir, LocalIdentity::generate())?;
        delivery.configure_local(DeliveryConfig {
            enabled: true,
            ..Default::default()
        })?;
        let mut stored = super::super::store::tests::paid_order(Role::Receiver);
        stored.order = fixture.order;
        stored.view = view(
            &uuid::Uuid::new_v4().to_string(),
            Role::Receiver,
            &stored.order,
            content.len() as u64,
        )?;
        stored.accepted = true;
        delivery.with_store(|store| {
            store.save_order(&stored)?;
            for (index, encrypted) in fixture.encrypted.iter().enumerate() {
                store.write_object(
                    &stored.view.order_id,
                    index as u16,
                    "ciphertext",
                    &encrypted.ciphertext,
                )?;
                store.write_object(
                    &stored.view.order_id,
                    index as u16,
                    "capsule",
                    &encrypted.capsule,
                )?;
            }
            Ok(())
        })?;
        assert!(
            delivery
                .read(&stored)
                .unwrap_err()
                .is::<ReceivingKeyUnavailable>()
        );
        let key = format!(
            "receiving-key:{}",
            hex::encode(stored.order.unsigned.terms.binding.key_id)
        );
        let mut material = ReceivingMaterial {
            binding: SignedKeyBinding {
                binding: stored.order.unsigned.terms.binding.clone(),
                signature: stored.order.binding_signature,
            },
            secret: Some(Secret32::new([7; 32])?),
        };
        delivery.with_store(|store| store.put_value(&key, &serde_json::to_vec(&material)?))?;
        let recovered = delivery.read(&stored)?;
        assert_eq!(recovered["name"], "restored.bin");
        assert_eq!(recovered["content_base64"], STANDARD.encode(content));
        assert_eq!(recovered["sha256"], hex::encode(hash(content)));
        stored.view.name = Some(text(&recovered, "name")?.into());
        stored.view.sha256 = Some(text(&recovered, "sha256")?.into());
        stored.view.evidence = Evidence::Recoverable;
        stored.view.phase = Phase::Paid;
        stored.view.payment.settlement = Some(BudgetSettlement {
            earned: "7".into(),
            withdrawn: "7".into(),
            refunded: "0".into(),
            publication: None,
        });
        stored.settlement_complete = true;
        for window in &mut stored.view.windows {
            window.state = WindowState::Earned;
            assert!(!window.decryption_proven);
        }
        assert!(
            stored.historical(),
            "actual local recovery does not require automatic Open"
        );
        assert!(
            delivery
                .with_store(|store| store.recent_transactions())?
                .is_empty()
        );
        assert!(
            delivery
                .with_store(|store| store.values_with_prefix("proof-seal:"))?
                .is_empty()
        );
        material.secret = None;
        delivery.with_store(|store| store.put_value(&key, &serde_json::to_vec(&material)?))?;
        assert!(
            delivery
                .read(&stored)
                .unwrap_err()
                .is::<ReceivingKeyUnavailable>()
        );
        drop(delivery);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn canonical_reservation_recovers_both_original_orders_after_quote_expiry_without_new_payment()
    -> Result<()> {
        let mut config = chain::tests::config();
        config.escrow = format!("0x{}", "44".repeat(20));
        config.asset = format!("0x{}", "33".repeat(20));
        let original = support::fixture_with_profile(&[4], config.profile.clone());
        let mut orders = Vec::new();
        for nonce in [1, 2, 3] {
            let mut terms = original.order.unsigned.terms.clone();
            terms.payer_order_nonce = [nonce; 32];
            let fixture = support::fixture_from_terms(
                config.profile.clone(),
                terms,
                vec![SecretBytes::new(vec![nonce; 4])],
                Amount::from_u64(2),
                Amount::from_u64(1),
            );
            let order = fixture.order;
            let intent = uuid::Uuid::new_v4().to_string();
            let mut view = view(&intent, Role::Sender, &order, 4)?;
            view.payment.budget_id = Some("77".repeat(32));
            view.chain_unknown();
            let mut source_seal = vec![0x22; 260];
            source_seal[..4].copy_from_slice(&[0x73, 0xc4, 0x57, 0xba]);
            orders.push(StoredOrder {
                network_id: Some("fixture-network".into()),
                authorized_by: Some("fixture-member".into()),
                confirmed: true,
                resume_requested: true,
                request_hash: "88".repeat(32),
                view,
                order,
                wallet_signature: vec![0x11; 65],
                source_seals: vec![source_seal],
                grants: Vec::new(),
                accepted: false,
                notified: false,
                allocation_batch: Vec::new(),
                chain_observed_at_ms: 0,
                settlement_complete: false,
            });
        }
        let ids: Vec<_> = orders[..2]
            .iter()
            .map(|order| order.view.order_id.clone())
            .collect();
        for order in &mut orders[..2] {
            order.allocation_batch = ids.clone();
        }
        let budget_id = B256::repeat_byte(0x77);
        let mut responses = BTreeMap::<String, Value>::new();
        let encoded = |value: Vec<u8>| json!(format!("0x{}", hex::encode(value)));
        for (index, order) in orders.iter().enumerate() {
            let id = B256::from(parse_word(&order.view.order_id)?);
            responses.insert(
                hex::encode(chain::abi::orderBudgetCall { id }.abi_encode()),
                encoded(if index < 2 { budget_id } else { B256::ZERO }.abi_encode()),
            );
            if index >= 2 {
                continue;
            }
            let statement = Statement::window(&order.order.unsigned, 0, Relation::Source)?;
            let terms = chain::abi::DeliveryStatement::abi_decode(statement.abi_bytes())?;
            let spec = &order.order.unsigned.windows[0];
            let state = chain::abi::ChainOrder {
                terms,
                windows: vec![chain::abi::ChainWindow {
                    spec: chain::abi::WindowSpec {
                        index: 0,
                        offset: 0,
                        plaintextBytes: 4,
                        ciphertextBytes: 72,
                        ciphertextHash: spec.ciphertext_hash.into(),
                        capsuleHash: spec.capsule_hash.into(),
                        plaintextCommitment: spec.plaintext_commitment.into(),
                        deliveryFee: U256::from(2),
                        reservationFee: U256::from(1),
                    },
                    phase: 0,
                    decryptionProven: false,
                    capsule: Vec::new().into(),
                }],
                earned: U256::ZERO,
                withdrawn: U256::ZERO,
                refunded: U256::ZERO,
                earnedWindows: 0,
                refundedWindows: 0,
                assetDecimals: 6,
                bonusEarned: false,
                bonusRefunded: false,
            };
            responses.insert(
                hex::encode(chain::abi::getOrderCall { id }.abi_encode()),
                encoded(chain::abi::getOrderCall::abi_encode_returns(&state)),
            );
            responses.insert(
                hex::encode(
                    chain::abi::publicValuesCall {
                        id,
                        index: 0,
                        relation: 2,
                    }
                    .abi_encode(),
                ),
                encoded(chain::abi::publicValuesCall::abi_encode_returns(
                    &chain::abi::DeliveryStatement::abi_decode(statement.abi_bytes())?,
                )),
            );
        }
        let budget = chain::abi::ChainBudget {
            payer: Address::repeat_byte(0x11),
            payee: Address::repeat_byte(0x22),
            providerPeer: orders[0].order.unsigned.terms.provider_peer.into(),
            asset: Address::repeat_byte(0x33),
            deposit: U256::from(8),
            locked: U256::from(8),
            earned: U256::ZERO,
            withdrawn: U256::ZERO,
            refunded: U256::ZERO,
            maxClaimUntil: 400,
            assetDecimals: 6,
            state: 1,
        };
        responses.insert(
            hex::encode(chain::abi::getBudgetCall { id: budget_id }.abi_encode()),
            encoded(chain::abi::getBudgetCall::abi_encode_returns(&budget)),
        );
        let rpc = chain::tests::rpc_fixture(config, move |request| match request["method"].as_str().unwrap() {
            "eth_blockNumber" => json!("0xb"),
            "eth_getBlockByNumber" => json!({"number":"0xa","hash":format!("0x{}","55".repeat(32)),"timestamp":"0xc9"}),
            "eth_call" => responses.get(request["params"][0]["data"].as_str().unwrap().strip_prefix("0x").unwrap()).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        }).await?;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("allocation-recovery-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let delivery = Delivery::open(&dir, LocalIdentity::generate())?;
        delivery.configure_local(DeliveryConfig {
            enabled: true,
            paid_enabled: true,
            chain: Some(rpc.chain.config().clone()),
            ..Default::default()
        })?;
        for order in &orders {
            delivery.with_store(|store| store.save_order(order))?;
        }
        let mut intent = chain::tests::approval_intent(Address::repeat_byte(0x11))?;
        intent.id = format!(
            "budget-{}/reserve-{}",
            "77".repeat(32),
            hex::encode(hash(&serde_json::to_vec(&ids)?))
        );
        let (target, calldata) = budget::BudgetAction::Reserve {
            budget_id: budget_id.into(),
            reservations: orders[..2]
                .iter()
                .map(|order| budget::Reservation {
                    order: order.order.clone(),
                    source_seals: order.source_seals.clone(),
                    wallet_signature: order.wallet_signature.clone(),
                })
                .collect(),
        }
        .encode(rpc.chain.config(), Address::repeat_byte(0x11))?;
        intent.to = chain::address_hex(target);
        intent.calldata = format!("0x{}", hex::encode(calldata));
        let transaction_id = intent.id.clone();
        let position = rpc.chain.confirmed_position().await?;
        let budget_snapshot = rpc.chain.budget_at(&position, budget_id).await?;
        rpc.chain.verify_position(&position).await?;
        delivery.with_store(|store| {
            store.put_value(
                &format!("protected-budget:{}", "77".repeat(32)),
                &serde_json::to_vec(&budget::BudgetRecord {
                    terms: budget_snapshot.terms,
                    payer_gas_cap: intent.gas_cap.clone(),
                    provider_gas_cap: "0".into(),
                    network_id: orders[0].network_id.clone(),
                    authorized_by: orders[0].authorized_by.clone(),
                    role: Role::Sender,
                })?,
            )?;
            store.reserve_budget_transaction(
                &intent,
                7,
                &format!("budget-{}", "77".repeat(32)),
                &intent.gas_cap,
            )?;
            store.sign_transaction(&transaction_id, "0x02", &format!("0x{}", "99".repeat(32)))?;
            store.observe_transaction(
                &transaction_id,
                "unknown",
                None,
                None,
                Some("synthetic lost allocation response"),
            )
        })?;
        let before = delivery.with_store(|store| store.transaction(&transaction_id))?;
        let mut recovering = orders[0].clone();
        assert_eq!(
            delivery
                .recover_allocation(&rpc.chain, &mut recovering)
                .await?,
            AllocationRecovery::Allocated
        );
        for id in &ids {
            let restored = delivery.with_store(|store| store.order(id))?;
            assert_eq!(restored.view.phase, Phase::Funded);
            assert_eq!(restored.allocation_batch, ids);
            assert_eq!(restored.view.payment.tx_hash, before.tx_hash);
            assert_eq!(restored.source_seals, orders[0].source_seals);
            assert_eq!(restored.order.unsigned.terms.quote_expires_at, 200);
        }
        assert_eq!(
            delivery.with_store(|store| store.transaction(&transaction_id))?,
            before,
            "restoring canonical allocations must preserve the original nonce, raw, cap and transaction identity"
        );
        let mut expired = orders[2].clone();
        assert!(
            delivery
                .recover_allocation(&rpc.chain, &mut expired)
                .await
                .unwrap_err()
                .to_string()
                .contains("no new allocation")
        );
        assert_eq!(
            delivery
                .with_store(|store| store.order(&expired.view.order_id))?
                .view
                .phase,
            Phase::Reconciling
        );
        assert!(rpc.requests.lock().unwrap().iter().all(|request| matches!(
            request["method"].as_str(),
            Some("eth_blockNumber" | "eth_getBlockByNumber" | "eth_call")
        )));
        let database = rusqlite::Connection::open_with_flags(
            dir.join("delivery/service.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        assert_eq!(
            database.query_row("SELECT count(*) FROM transactions", [], |row| row
                .get::<_, i64>(0))?,
            1
        );
        drop(database);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
