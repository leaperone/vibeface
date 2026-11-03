use super::*;
use orders::{Quotation, address_bytes, view};

impl Delivery {
    pub(super) async fn handle_service(
        &self,
        node: &Runtime,
        remote: &str,
        request: ServiceRequest,
    ) -> Result<Value> {
        ensure!(self.config().enabled, "object delivery service is disabled");
        let _maintenance = node.maintenance.enter()?;
        let _provider = if matches!(&request, ServiceRequest::Charity { .. }) {
            None
        } else {
            Some(self.provider_operations.lock().await)
        };
        match request {
            ServiceRequest::Charity { request } => {
                self.charity_handle_service(node, remote, request).await
            }
            ServiceRequest::Announce => {
                self.ensure_mode(Mode::Paid)?;
                let config = self.config();
                ensure!(
                    Amount::from_decimal_str(&config.provider_gas_cap)? != Amount::ZERO,
                    "paid provider requires an explicit settlement gas budget"
                );
                let (chain_id, escrow) = deployment(&config, Mode::Paid)?;
                let chain = config.chain.as_ref().context("paid deployment missing")?;
                let provider_wallet = format!(
                    "0x{}",
                    hex::encode(
                        self.wallet
                            .lock()
                            .expect("delivery wallet")
                            .address()?
                            .as_slice()
                    )
                );
                Ok(
                    json!({"peer_id":self.identity.device_id()?,"paid_enabled":true,
                    "profile":chain.profile,"provider_wallet":provider_wallet,"asset":chain.asset,
                    "chain_id":chain_id,"escrow":escrow,"delivery_fee":config.delivery_fee_per_window,
                    "reservation_fee":config.reservation_fee_per_window,"priority_fee":config.priority_fee}),
                )
            }
            ServiceRequest::KeyBinding { mode } => {
                ensure!(
                    mode == Mode::Paid,
                    "charity uses its independent key binding"
                );
                self.config()
                    .prover
                    .context("receiving Node requires a trusted local prover")?
                    .readiness()?;
                let binding = self.key_binding(mode)?;
                if mode == Mode::Paid {
                    let current = self
                        .chain()
                        .await?
                        .current_key(binding.binding.receiver_peer)
                        .await?;
                    ensure!(
                        current.active
                            && current.serial == binding.binding.serial
                            && current.binding_hash == binding.binding.digest()?,
                        "receiving Node must register its delivery key locally before paid delivery"
                    );
                }
                Ok(serde_json::to_value(binding)?)
            }
            ServiceRequest::Quote {
                mode,
                order,
                binding,
                file_bytes,
                intent_id,
            } => {
                self.ensure_mode(mode)?;
                ensure!(
                    Amount::from_decimal_str(&self.config().provider_gas_cap)? != Amount::ZERO,
                    "paid provider requires an explicit settlement gas budget"
                );
                binding.verify()?;
                order.validate()?;
                let terms = &order.terms;
                let provider = self.identity.device_id()?;
                let config = self.config();
                ensure!(
                    hex::encode(terms.source_peer) == remote
                        && hex::encode(terms.provider_peer) == provider,
                    "quote names a different sender or provider"
                );
                ensure!(
                    binding.binding == terms.binding
                        && file_bytes <= MAX_FILE_BYTES as u64
                        && order.windows.len() <= 2,
                    "invalid quoted file or receiving binding"
                );
                uuid::Uuid::parse_str(&intent_id)?;
                let (chain_id, escrow) = deployment(&config, mode)?;
                ensure!(
                    terms.binding.chain_id == chain_id && terms.binding.escrow == escrow,
                    "quote belongs to another deployment"
                );
                let profile = config
                    .chain
                    .as_ref()
                    .context("paid deployment missing")?
                    .profile
                    .clone();
                profile.check_order(&order)?;
                let policy = paid_deadlines(now(), &profile);
                let budget = policy.hold_deadline - policy.quote_expires_at;
                ensure!(
                    terms.quote_expires_at >= now()
                        && terms.quote_expires_at <= policy.quote_expires_at
                        && terms.hold_deadline - terms.quote_expires_at == budget
                        && terms.key_deadline - terms.hold_deadline == 600
                        && terms.open_deadline - terms.key_deadline == budget
                        && terms.retain_until - terms.open_deadline == 86400
                        && terms.priority_deadline - terms.quote_expires_at == 3600,
                    "service deadlines differ from its signed policy"
                );
                let (delivery, reservation, bonus) = if mode == Mode::Paid {
                    (
                        Amount::from_decimal_str(&config.delivery_fee_per_window)?,
                        Amount::from_decimal_str(&config.reservation_fee_per_window)?,
                        Amount::from_decimal_str(&config.priority_fee)?,
                    )
                } else {
                    (Amount::ZERO, Amount::ZERO, Amount::ZERO)
                };
                ensure!(terms.whole_order_bonus==bonus && order.windows.iter().all(|w|w.delivery_fee==delivery && w.reservation_fee==reservation),"quote fees differ from service policy");
                let id = hex::encode(terms.order_id()?);
                if let Some(prior) = self.with_store(|store| store.find_order(&id))? {
                    ensure!(
                        prior.order.unsigned == order,
                        "quoted order was changed across retry"
                    );
                    return Ok(serde_json::to_value(Quotation {
                        signature: prior.order.quote_signature,
                        wallet_signature: prior.wallet_signature,
                        grants: prior.grants,
                    })?);
                }
                let wallet_signature = if mode == Mode::Paid {
                    let chain = config.chain.as_ref().context("paid deployment missing")?;
                    ensure!(
                        terms.asset == address_bytes(&chain.asset)?
                            && terms.provider_wallet
                                == self
                                    .wallet
                                    .lock()
                                    .expect("delivery wallet")
                                    .address()?
                                    .as_slice(),
                        "quote asset or provider wallet differs from local configuration"
                    );
                    let current = self
                        .chain()
                        .await?
                        .current_key(terms.binding.receiver_peer)
                        .await?;
                    ensure!(
                        current.active
                            && current.serial == terms.binding.serial
                            && current.binding_hash == terms.binding.digest()?,
                        "quote receiving key was rotated or revoked"
                    );
                    self.wallet.lock().expect("delivery wallet").sign_quote(
                        chain,
                        order.quote_digest()?,
                        terms.provider_peer,
                    )?
                } else {
                    Vec::new()
                };
                let quote_signature = signature(
                    self.identity
                        .sign_payload("delivery-quote", &order.quote_signing_payload()?)?,
                )?;
                let placeholder = Order {
                    unsigned: order,
                    binding_signature: binding.signature,
                    quote_signature,
                    source_signature: vibeface_delivery::PeerSignature {
                        r: [0; 32],
                        s: [0; 32],
                    },
                };
                let mut grants = Vec::new();
                for window in &placeholder.unsigned.windows {
                    for operation in [GrantOperation::PutCiphertext, GrantOperation::PutCapsule] {
                        grants.push(self.issue_grant(
                            &placeholder,
                            remote,
                            window.index,
                            operation,
                        )?);
                    }
                }
                let stored = StoredOrder {
                    network_id: None,
                    authorized_by: None,
                    confirmed: false,
                    resume_requested: false,
                    request_hash: String::new(),
                    view: view(&intent_id, Role::Provider, mode, &placeholder, file_bytes)?,
                    order: placeholder,
                    wallet_signature: wallet_signature.clone(),
                    source_seals: Vec::new(),
                    grants: grants.clone(),
                    accepted: false,
                    notified: false,
                };
                self.with_store(|store| {
                    store.save_order(&stored)?;
                    for window in &stored.order.unsigned.windows {
                        store.reserve_object(
                            &id,
                            window.index,
                            "ciphertext",
                            &hex::encode(window.ciphertext_hash),
                            window.ciphertext_bytes,
                        )?;
                        store.reserve_object(
                            &id,
                            window.index,
                            "capsule",
                            &hex::encode(window.capsule_hash),
                            80,
                        )?;
                    }
                    Ok(())
                })?;
                Ok(serde_json::to_value(Quotation {
                    signature: quote_signature,
                    wallet_signature,
                    grants,
                })?)
            }
            ServiceRequest::Offer {
                mode,
                order,
                file_bytes,
                intent_id,
                wallet_signature,
            } => {
                self.ensure_mode(mode)?;
                order.verify()?;
                ensure!(
                    hex::encode(order.unsigned.terms.source_peer) == remote
                        && file_bytes <= MAX_FILE_BYTES as u64,
                    "delivery offer has a different source or exceeds capacity"
                );
                let id = hex::encode(order.unsigned.terms.order_id()?);
                let own = self.identity.device_id()?;
                let role = if hex::encode(order.unsigned.terms.provider_peer) == own {
                    Role::Provider
                } else {
                    Role::Receiver
                };
                if role == Role::Receiver {
                    ensure!(
                        hex::encode(order.unsigned.terms.binding.receiver_peer) == own,
                        "delivery offer belongs to another receiver"
                    );
                    let key = self.key_binding(mode)?;
                    ensure!(
                        key.binding == order.unsigned.terms.binding,
                        "receiving key changed before offer was accepted"
                    );
                }
                if mode == Mode::Paid {
                    self.paid_state(&order).await?;
                }
                if let Some(mut prior) = self.with_store(|store| store.find_order(&id))? {
                    ensure!(
                        prior.order.unsigned == order.unsigned,
                        "delivery offer changed across retry"
                    );
                    prior.order = order;
                    if prior.view.role == Role::Provider {
                        prior.accepted = true;
                    }
                    self.save(&mut prior)?;
                    return Ok(json!({"order_id":id}));
                }
                ensure!(
                    role == Role::Receiver,
                    "provider must issue a quotation before accepting an order"
                );
                let network = node.membership_state().await?.genesis.network_id;
                let mut stored = StoredOrder {
                    network_id: Some(network),
                    authorized_by: None,
                    confirmed: false,
                    resume_requested: false,
                    request_hash: String::new(),
                    view: view(&intent_id, role, mode, &order, file_bytes)?,
                    order,
                    wallet_signature,
                    source_seals: Vec::new(),
                    grants: Vec::new(),
                    accepted: false,
                    notified: false,
                };
                stored.view.phase = Phase::Funded;
                self.save(&mut stored)?;
                Ok(json!({"order_id":id}))
            }
            ServiceRequest::Grant {
                order_id,
                index,
                operation,
            } => {
                let stored = self.with_store(|store| store.order(&order_id))?;
                self.authorize_object(remote, &stored, index, &operation)
                    .await?;
                Ok(serde_json::to_value(self.issue_grant(
                    &stored.order,
                    remote,
                    index,
                    operation,
                )?)?)
            }
            ServiceRequest::Offset { grant } => {
                let (stored, kind) = self.check_grant(remote, &grant).await?;
                let offset = self.with_store(|store| {
                    store.reserve_object(
                        &stored.view.order_id,
                        grant.body.window_index,
                        kind,
                        &grant.body.object_hash,
                        grant.body.max_bytes,
                    )
                })?;
                Ok(json!({"offset":offset}))
            }
            ServiceRequest::Put {
                grant,
                offset,
                bytes,
            } => {
                ensure!(
                    matches!(
                        grant.body.operation,
                        GrantOperation::PutCiphertext | GrantOperation::PutCapsule
                    ),
                    "service grant does not permit uploading"
                );
                ensure!(
                    bytes.len() <= CHUNK_BYTES.div_ceil(3) * 4,
                    "delivery chunk exceeds limit"
                );
                let bytes = STANDARD.decode(bytes)?;
                let (mut stored, kind) = self.check_grant(remote, &grant).await?;
                let next = self.with_store(|store| {
                    store.append_object(
                        &stored.view.order_id,
                        grant.body.window_index,
                        kind,
                        offset,
                        &bytes,
                    )
                })?;
                if kind == "ciphertext" {
                    let window = &mut stored.view.windows[grant.body.window_index as usize];
                    window.committed_bytes = next;
                    if next == grant.body.max_bytes {
                        window.state = WindowState::Stored;
                    }
                    stored.view.phase = Phase::Transferring;
                    self.save(&mut stored)?;
                }
                Ok(json!({"offset":next}))
            }
            ServiceRequest::Get { grant, offset } => {
                ensure!(
                    matches!(
                        grant.body.operation,
                        GrantOperation::GetCiphertext | GrantOperation::GetCapsule
                    ),
                    "service grant does not permit reading"
                );
                let (stored, kind) = self.check_grant(remote, &grant).await?;
                let bytes = self.with_store(|store| {
                    store.object(&stored.view.order_id, grant.body.window_index, kind)
                })?;
                ensure!(
                    offset as usize <= bytes.len(),
                    "delivery read offset exceeds object"
                );
                let end = (offset as usize + CHUNK_BYTES).min(bytes.len());
                Ok(
                    json!({"bytes":STANDARD.encode(&bytes[offset as usize..end]),"next_offset":end,"complete":end==bytes.len()}),
                )
            }
            ServiceRequest::Hold {
                order_id,
                index,
                consent,
                seal,
            } => {
                let mut stored = self.with_store(|store| store.order(&order_id))?;
                ensure!(
                    stored.view.role == Role::Provider && stored.view.receiver_peer == remote,
                    "only the designated receiver can submit a hold"
                );
                let statement = Statement::window(&stored.order.unsigned, index, Relation::Hold)?;
                vibeface_delivery::verify_peer(
                    &stored.order.unsigned.terms.binding.receiver_peer,
                    "delivery-accept",
                    &statement.consent_signing_payload()?,
                    &consent,
                )?;
                self.with_store(|store| store.object(&order_id, index, "ciphertext"))?;
                if stored.view.mode == Mode::Paid {
                    let chain = self.chain().await?;
                    let observed = chain
                        .execute(
                            self,
                            &format!("{order_id}/hold-{index}"),
                            chain::Call::Hold {
                                order_id: parse_word(&order_id)?,
                                index,
                                seal,
                            },
                            &self.config().provider_gas_cap,
                        )
                        .await?;
                    if observed.phase != "confirmed" {
                        stored.view.chain_unknown();
                        self.save(&mut stored)?;
                        return Ok(serde_json::to_value(stored.view)?);
                    }
                    let capsule =
                        self.with_store(|store| store.object(&order_id, index, "capsule"))?;
                    let observed = chain
                        .execute(
                            self,
                            &format!("{order_id}/publish-{index}"),
                            chain::Call::Publish {
                                order_id: parse_word(&order_id)?,
                                index,
                                capsule,
                            },
                            &self.config().provider_gas_cap,
                        )
                        .await?;
                    if observed.phase != "confirmed" {
                        stored.view.chain_unknown();
                        self.save(&mut stored)?;
                        return Ok(serde_json::to_value(stored.view)?);
                    }
                    self.update_paid_view(&mut stored).await?;
                    self.withdraw_earned(&mut stored, index).await?;
                } else {
                    ensure!(
                        seal.is_empty(),
                        "charity receipts cannot claim a paid proof"
                    );
                    stored
                        .view
                        .windows
                        .get_mut(index as usize)
                        .context("invalid window")?
                        .state = WindowState::Held;
                    stored.view.evidence = Evidence::CharityReceipt;
                    stored.view.phase = Phase::CipherHeld;
                }
                self.save(&mut stored)?;
                Ok(serde_json::to_value(stored.view)?)
            }
            ServiceRequest::Open {
                order_id,
                index,
                seal,
            } => {
                let mut stored = self.with_store(|store| store.order(&order_id))?;
                ensure!(
                    stored.view.mode == Mode::Paid && stored.view.receiver_peer == remote,
                    "open proof requires the paid designated receiver"
                );
                let observed = self
                    .chain()
                    .await?
                    .execute(
                        self,
                        &format!("{order_id}/open-{index}"),
                        chain::Call::Open {
                            order_id: parse_word(&order_id)?,
                            index,
                            seal,
                        },
                        &self.config().provider_gas_cap,
                    )
                    .await?;
                if observed.phase == "confirmed" {
                    self.update_paid_view(&mut stored).await?;
                    self.withdraw_earned(&mut stored, index).await?;
                } else {
                    stored.view.chain_unknown();
                }
                self.save(&mut stored)?;
                Ok(serde_json::to_value(stored.view)?)
            }
            ServiceRequest::Order { order_id } => {
                let mut stored = self.with_store(|store| store.order(&order_id))?;
                ensure!(
                    remote == stored.view.source_peer || remote == stored.view.receiver_peer,
                    "delivery order belongs to another peer"
                );
                if stored.view.mode == Mode::Paid {
                    self.update_paid_view(&mut stored).await?;
                    self.save(&mut stored)?;
                }
                Ok(serde_json::to_value(stored.view)?)
            }
        }
    }
    fn issue_grant(
        &self,
        order: &Order,
        remote: &str,
        index: u16,
        operation: GrantOperation,
    ) -> Result<ServiceGrant> {
        let terms = &order.unsigned.terms;
        let window = order
            .unsigned
            .windows
            .get(index as usize)
            .context("invalid delivery window")?;
        let capsule = matches!(
            operation,
            GrantOperation::PutCapsule | GrantOperation::GetCapsule
        );
        let body = GrantBody {
            provider_peer: self.identity.device_id()?,
            client_peer: remote.into(),
            order_id: hex::encode(terms.order_id()?),
            window_index: index,
            object_hash: hex::encode(if capsule {
                window.capsule_hash
            } else {
                window.ciphertext_hash
            }),
            operation,
            max_bytes: if capsule { 80 } else { window.ciphertext_bytes },
            expires_at: terms.retain_until,
        };
        let signature = self
            .identity
            .sign_payload("delivery-grant", &serde_json::to_vec(&body)?)?;
        Ok(ServiceGrant { body, signature })
    }
    async fn check_grant(
        &self,
        remote: &str,
        grant: &ServiceGrant,
    ) -> Result<(StoredOrder, &'static str)> {
        grant.verify(&self.identity.device_id()?, remote, now())?;
        let stored = self.with_store(|store| store.order(&grant.body.order_id))?;
        self.authorize_object(
            remote,
            &stored,
            grant.body.window_index,
            &grant.body.operation,
        )
        .await?;
        let expected = self.issue_grant(
            &stored.order,
            remote,
            grant.body.window_index,
            grant.body.operation.clone(),
        )?;
        ensure!(
            expected.body == grant.body,
            "service grant object or quota differs from its order"
        );
        let kind = if matches!(
            grant.body.operation,
            GrantOperation::PutCapsule | GrantOperation::GetCapsule
        ) {
            "capsule"
        } else {
            "ciphertext"
        };
        Ok((stored, kind))
    }
    async fn authorize_object(
        &self,
        remote: &str,
        stored: &StoredOrder,
        index: u16,
        operation: &GrantOperation,
    ) -> Result<()> {
        self.ensure_mode(stored.view.mode)?;
        stored.order.verify()?;
        ensure!(
            stored.view.role == Role::Provider && now() <= stored.order.unsigned.terms.retain_until,
            "provider order is unavailable"
        );
        let upload = matches!(
            operation,
            GrantOperation::PutCiphertext | GrantOperation::PutCapsule
        );
        ensure!(
            remote
                == if upload {
                    &stored.view.source_peer
                } else {
                    &stored.view.receiver_peer
                },
            "service grant client has the wrong role"
        );
        let window = stored
            .view
            .windows
            .get(index as usize)
            .context("invalid delivery window")?;
        if stored.view.mode == Mode::Paid {
            let chain = self.chain().await?;
            let state = self.paid_state(&stored.order).await?;
            let state = state
                .windows
                .get(index as usize)
                .context("chain window is missing")?;
            ensure!(state.phase != 4, "refunded service window is terminal");
            if state.phase == 0 {
                let key = chain
                    .current_key(stored.order.unsigned.terms.binding.receiver_peer)
                    .await?;
                ensure!(
                    key.active
                        && key.serial == stored.order.unsigned.terms.binding.serial
                        && key.binding_hash == stored.order.unsigned.terms.binding.digest()?,
                    "receiving key was rotated or revoked"
                );
            }
            if matches!(operation, GrantOperation::GetCapsule) {
                ensure!(
                    state.phase == 2 || state.phase == 3,
                    "key capsule is unavailable until its publication is confirmed"
                );
            }
        } else if matches!(operation, GrantOperation::GetCapsule) {
            ensure!(
                matches!(
                    window.state,
                    WindowState::Held | WindowState::KeyPublished | WindowState::Earned
                ),
                "charity capsule requires the receiver's signed receipt"
            );
        }
        Ok(())
    }
    pub(super) async fn paid_state(&self, order: &Order) -> Result<chain::OrderState> {
        let chain = self.chain().await?;
        let id = order.unsigned.terms.order_id()?;
        let position = chain.confirmed_position().await?;
        let state = chain.order(id).await?;
        ensure!(
            state.windows.len() == order.unsigned.windows.len()
                && state.payer == format!("0x{}", hex::encode(order.unsigned.terms.payer))
                && state.provider_wallet
                    == format!("0x{}", hex::encode(order.unsigned.terms.provider_wallet)),
            "chain order differs from its signed manifest"
        );
        for window in &order.unsigned.windows {
            ensure!(
                chain
                    .public_values_at(&position, id, window.index, Relation::Source)
                    .await?
                    == Statement::window(&order.unsigned, window.index, Relation::Source)?,
                "chain public values differ from the signed order"
            );
        }
        chain.verify_position(&position).await?;
        Ok(state)
    }
    pub(super) async fn run_provider(&self) -> Result<()> {
        if !self.config().paid_enabled {
            return Ok(());
        }
        let Ok(_provider) = self.provider_operations.try_lock() else {
            return Ok(());
        };
        let mut orders = self.with_store(|store| store.orders(None))?;
        orders.retain(|order| {
            order.view.role == Role::Provider && order.view.mode == Mode::Paid && order.accepted
        });
        let transaction = self.with_store(|store| store.next_transaction_for_reconcile())?;
        if orders.is_empty() && transaction.is_none() {
            return Ok(());
        }
        let chain = self.chain().await?;
        if let Some(transaction) = transaction {
            let observed = chain
                .reconcile_transaction(self, &transaction.intent.id)
                .await?;
            self.finish_key_operation(&chain, &transaction.intent.id, &observed.phase)
                .await?;
        }
        if orders.is_empty() {
            return Ok(());
        }
        orders.sort_by_key(|order| order.view.updated_at_ms);
        let mut order = orders.remove(0);
        match self.update_paid_view(&mut order).await {
            Ok(()) => order.view.error = None,
            Err(error) => {
                order.view.chain_unknown();
                order.view.error = Some(error.to_string());
                self.save(&mut order)?;
                return Ok(());
            }
        }
        if let Some(window) = order
            .view
            .windows
            .iter()
            .find(|window| window.state == WindowState::Held)
        {
            let index = window.index;
            let capsule =
                self.with_store(|store| store.object(&order.view.order_id, index, "capsule"))?;
            let result = chain
                .execute(
                    self,
                    &format!("{}/publish-{index}", order.view.order_id),
                    chain::Call::Publish {
                        order_id: order.order.unsigned.terms.order_id()?,
                        index,
                        capsule,
                    },
                    &self.config().provider_gas_cap,
                )
                .await;
            if let Err(error) = result {
                order.view.error = Some(error.to_string());
            }
        } else if order.view.payment.earned != order.view.payment.withdrawn
            && let Some(window) = order
                .view
                .windows
                .iter()
                .rev()
                .find(|window| window.state == WindowState::Earned)
        {
            let index = window.index;
            if let Err(error) = self.withdraw_earned(&mut order, index).await {
                order.view.error = Some(error.to_string());
            }
        }
        self.save(&mut order)
    }
    async fn withdraw_earned(&self, stored: &mut StoredOrder, index: u16) -> Result<()> {
        let earned = Amount::from_decimal_str(&stored.view.payment.earned)?;
        let withdrawn = Amount::from_decimal_str(&stored.view.payment.withdrawn)?;
        if earned.to_be_bytes() <= withdrawn.to_be_bytes() {
            return Ok(());
        }
        let observed = self
            .chain()
            .await?
            .execute(
                self,
                &format!("{}/withdraw-{index}", stored.view.order_id),
                chain::Call::Withdraw {
                    order_id: stored.order.unsigned.terms.order_id()?,
                },
                &self.config().provider_gas_cap,
            )
            .await?;
        if observed.phase == "confirmed" {
            self.update_paid_view(stored).await?;
        }
        Ok(())
    }
    pub(super) async fn update_paid_view(&self, stored: &mut StoredOrder) -> Result<()> {
        let state = self.paid_state(&stored.order).await?;
        apply_paid_snapshot(&mut stored.view, &state)
    }
}

fn apply_paid_snapshot(view: &mut DeliveryOrder, state: &chain::OrderState) -> Result<()> {
    ensure!(
        !state.windows.is_empty() && state.windows.len() == view.windows.len(),
        "chain delivery window coverage changed"
    );
    view.payment.earned = state.earned.clone();
    view.payment.withdrawn = state.withdrawn.clone();
    view.payment.refunded = state.refunded.clone();
    view.phase = Phase::Funded;
    view.payment.state = PaymentState::Confirmed;
    view.evidence = Evidence::None;
    for (window, fact) in view.windows.iter_mut().zip(&state.windows) {
        window.state = match fact.phase {
            0 => WindowState::Pending,
            1 => WindowState::Held,
            2 => WindowState::KeyPublished,
            3 => WindowState::Earned,
            4 => WindowState::Refunded,
            _ => bail!("unknown chain delivery phase"),
        };
        window.decryption_proven = fact.decryption_proven;
    }
    let all_earned = state.windows.iter().all(|window| window.phase == 3);
    let all_refunded = state.windows.iter().all(|window| window.phase == 4);
    let any_refunded = state.windows.iter().any(|window| window.phase == 4);
    let all_recoverable = state.windows.iter().all(|window| {
        matches!(window.phase, 2..=4)
            && window.capsule.len() == 80
            && hash(&window.capsule) == window.capsule_hash
    });
    let all_held = state
        .windows
        .iter()
        .all(|window| matches!(window.phase, 1..=3));
    let any_progress = state.windows.iter().any(|window| window.phase != 0);
    if all_earned {
        view.phase = Phase::Paid;
    } else if all_refunded {
        view.phase = Phase::Refunded;
        view.payment.state = PaymentState::Refunded;
    } else if any_refunded {
        view.phase = Phase::Partial;
    } else if all_recoverable {
        view.phase = Phase::KeyPublished;
    } else if all_held {
        view.phase = Phase::CipherHeld;
    } else if any_progress {
        view.phase = Phase::Partial;
    }
    if view.payment.earned != "0" || all_earned {
        view.payment.state =
            if view.payment.withdrawn == view.payment.earned && view.payment.earned != "0" {
                PaymentState::Withdrawn
            } else {
                PaymentState::Earned
            };
    }
    view.evidence = if all_recoverable
        && state
            .windows
            .iter()
            .all(|window| window.phase == 3 && window.decryption_proven)
    {
        Evidence::DecryptionProven
    } else if all_recoverable {
        Evidence::Recoverable
    } else if all_held {
        Evidence::CiphertextHeld
    } else {
        Evidence::None
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> DeliveryOrder {
        DeliveryOrder {
            order_id: "ab".repeat(32),
            intent_id: uuid::Uuid::new_v4().to_string(),
            role: Role::Receiver,
            mode: Mode::Paid,
            acceptance: Acceptance::Strict,
            source_peer: "11".repeat(32),
            receiver_peer: "22".repeat(32),
            provider_peer: "33".repeat(32),
            name: None,
            bytes: 8,
            sha256: None,
            phase: Phase::Paid,
            evidence: Evidence::DecryptionProven,
            payment: Payment {
                state: PaymentState::Withdrawn,
                chain_id: Some("31337".into()),
                asset: Some(format!("0x{}", "44".repeat(20))),
                payer: Some(format!("0x{}", "55".repeat(20))),
                provider_wallet: Some(format!("0x{}", "66".repeat(20))),
                delivery_fee: "20".into(),
                reservation_fee: "4".into(),
                priority_fee: "5".into(),
                total: "29".into(),
                gas_cap: "1000000000000000".into(),
                earned: "29".into(),
                withdrawn: "29".into(),
                refunded: "0".into(),
                tx_hash: None,
            },
            windows: (0..2)
                .map(|index| WindowView {
                    index,
                    bytes: 4,
                    committed_bytes: 72,
                    state: WindowState::Earned,
                    decryption_proven: true,
                })
                .collect(),
            deadlines: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            error: None,
        }
    }
    fn fact(phase: u8, open: bool) -> chain::WindowState {
        let capsule = if matches!(phase, 2 | 3) {
            vec![0x42; 80]
        } else {
            Vec::new()
        };
        chain::WindowState {
            phase,
            decryption_proven: open,
            ciphertext_hash: [0xab; 32],
            capsule_hash: hash(&capsule),
            capsule,
        }
    }
    fn snapshot(
        windows: Vec<chain::WindowState>,
        earned: &str,
        withdrawn: &str,
        refunded: &str,
    ) -> chain::OrderState {
        chain::OrderState {
            payer: format!("0x{}", "55".repeat(20)),
            provider_wallet: format!("0x{}", "66".repeat(20)),
            earned: earned.into(),
            withdrawn: withdrawn.into(),
            refunded: refunded.into(),
            bonus_earned: false,
            bonus_refunded: false,
            windows,
        }
    }
    #[test]
    fn a_reorg_to_funded_rebuilds_money_and_proof_state_without_old_completion() {
        let mut view = view();
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(0, false), fact(0, false)], "0", "0", "0"),
        )
        .unwrap();
        assert_eq!(view.phase, Phase::Funded);
        assert_eq!(view.payment.state, PaymentState::Confirmed);
        assert_eq!(view.evidence, Evidence::None);
        assert_eq!(view.payment.earned, "0");
        assert_eq!(view.payment.withdrawn, "0");
        assert!(
            view.windows
                .iter()
                .all(|window| window.state == WindowState::Pending && !window.decryption_proven)
        );
        assert_eq!(view.windows[0].committed_bytes, 72);
    }
    #[test]
    fn mixed_strict_windows_never_claim_the_whole_file_is_recoverable() {
        let mut view = view();
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(2, false), fact(0, false)], "0", "0", "0"),
        )
        .unwrap();
        assert_eq!(view.phase, Phase::Partial);
        assert_eq!(view.evidence, Evidence::None);
        assert_eq!(view.windows[0].state, WindowState::KeyPublished);
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(3, true), fact(1, false)], "12", "0", "0"),
        )
        .unwrap();
        assert_eq!(view.evidence, Evidence::CiphertextHeld);
        assert_eq!(view.payment.state, PaymentState::Earned);
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(3, true), fact(4, false)], "12", "12", "17"),
        )
        .unwrap();
        assert_eq!(view.phase, Phase::Partial);
        assert_eq!(view.evidence, Evidence::None);
        assert_eq!(view.payment.state, PaymentState::Withdrawn);
    }
    #[test]
    fn whole_file_evidence_uses_every_current_window_and_refunds_clear_old_proofs() {
        let mut view = view();
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(2, false), fact(2, false)], "0", "0", "0"),
        )
        .unwrap();
        assert_eq!(view.phase, Phase::KeyPublished);
        assert_eq!(view.evidence, Evidence::Recoverable);
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(3, true), fact(3, true)], "29", "29", "0"),
        )
        .unwrap();
        assert_eq!(view.phase, Phase::Paid);
        assert_eq!(view.evidence, Evidence::DecryptionProven);
        apply_paid_snapshot(
            &mut view,
            &snapshot(vec![fact(4, false), fact(4, false)], "0", "0", "29"),
        )
        .unwrap();
        assert_eq!(view.phase, Phase::Refunded);
        assert_eq!(view.payment.state, PaymentState::Refunded);
        assert_eq!(view.evidence, Evidence::None);
    }
    #[test]
    fn an_unknown_chain_view_retracts_order_and_window_proof_claims() {
        let mut view = view();
        view.chain_unknown();
        assert_eq!(view.phase, Phase::Reconciling);
        assert_eq!(view.payment.state, PaymentState::Unknown);
        assert_eq!(view.evidence, Evidence::None);
        assert!(
            view.windows
                .iter()
                .all(|window| !window.decryption_proven && window.state == WindowState::Pending)
        );
        assert!(!matches!(
            view.phase,
            Phase::Paid | Phase::Refunded | Phase::Failed
        ));
    }
}
