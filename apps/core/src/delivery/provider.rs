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
        if !matches!(&request, ServiceRequest::Charity { .. }) {
            self.ensure_mode(Mode::Paid)?;
        }
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
                config
                    .publication_policy
                    .as_ref()
                    .context("paid provider requires an explicit local PublicationPolicy")?
                    .validate()?;
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
                {
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
                budget_id,
            } => {
                ensure!(
                    mode == Mode::Paid,
                    "charity uses its independent signed protocol"
                );
                ensure!(
                    Amount::from_decimal_str(&self.config().provider_gas_cap)? != Amount::ZERO,
                    "paid provider requires an explicit settlement gas budget"
                );
                binding.verify()?;
                order.validate()?;
                let terms = &order.terms;
                ensure!(
                    terms.acceptance == vibeface_delivery::Acceptance::Recoverable,
                    "protected Node delivery does not support Strict"
                );
                self.config()
                    .publication_policy
                    .as_ref()
                    .context("paid provider requires an explicit local PublicationPolicy")?
                    .validate()?;
                let prepaid = self.budget_snapshot(&budget_id).await?;
                ensure!(
                    prepaid.state == "open"
                        && prepaid.terms.payer == chain::address_hex(terms.payer.into())
                        && prepaid.terms.payee == chain::address_hex(terms.provider_wallet.into())
                        && prepaid.terms.provider_peer == hex::encode(terms.provider_peer)
                        && prepaid.terms.asset == chain::address_hex(terms.asset.into()),
                    "quote differs from its prepaid protected budget"
                );
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
                let (delivery, reservation, bonus) = (
                    Amount::from_decimal_str(&config.delivery_fee_per_window)?,
                    Amount::from_decimal_str(&config.reservation_fee_per_window)?,
                    Amount::from_decimal_str(&config.priority_fee)?,
                );
                ensure!(terms.whole_order_bonus==bonus && order.windows.iter().all(|w|w.delivery_fee==delivery && w.reservation_fee==reservation),"quote fees differ from service policy");
                let id = hex::encode(terms.order_id()?);
                if let Some(prior) = self.with_store(|store| store.find_order(&id))? {
                    ensure!(
                        prior.order.unsigned == order
                            && prior.view.payment.budget_id.as_deref() == Some(&budget_id),
                        "quoted order or budget was changed across retry"
                    );
                    return Ok(serde_json::to_value(Quotation {
                        signature: prior.order.quote_signature,
                        wallet_signature: prior.wallet_signature,
                        grants: prior.grants,
                    })?);
                }
                let wallet_signature = {
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
                };
                self.register_budget(&prepaid, "0")?;
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
                let mut stored = StoredOrder {
                    network_id: None,
                    authorized_by: None,
                    confirmed: false,
                    resume_requested: false,
                    request_hash: String::new(),
                    view: view(&intent_id, Role::Provider, &placeholder, file_bytes)?,
                    order: placeholder,
                    wallet_signature: wallet_signature.clone(),
                    source_seals: Vec::new(),
                    grants: grants.clone(),
                    accepted: false,
                    notified: false,
                    allocation_batch: Vec::new(),
                    chain_observed_at_ms: 0,
                    settlement_complete: false,
                };
                stored.view.payment.budget_id = Some(budget_id);
                self.with_store(|store| store.save_order(&stored))?;
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
                budget_id,
            } => {
                ensure!(
                    mode == Mode::Paid,
                    "charity uses its independent signed protocol"
                );
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
                        now() <= order.unsigned.terms.hold_deadline,
                        "delivery acceptance deadline expired"
                    );
                    ensure!(
                        hex::encode(order.unsigned.terms.binding.receiver_peer) == own,
                        "delivery offer belongs to another receiver"
                    );
                    let key_id = hex::encode(order.unsigned.terms.binding.key_id);
                    let material = Zeroizing::new(
                        self.with_store(|store| store.value(&format!("receiving-key:{key_id}")))?
                            .context("historical receiving key is unavailable")?,
                    );
                    let key: ReceivingMaterial = serde_json::from_slice(&material)?;
                    ensure!(
                        key.binding.binding == order.unsigned.terms.binding && key.secret.is_some(),
                        "historical receiving binding differs from the reserved order"
                    );
                }
                let (_, budget) = self.paid_state_with_budget(&order).await?;
                ensure!(
                    budget.terms.budget_id == budget_id,
                    "delivery offer names another protected allocation"
                );
                self.register_budget(&budget, "0")?;
                if let Some(mut prior) = self.with_store(|store| store.find_order(&id))? {
                    ensure!(
                        prior.order.unsigned == order.unsigned
                            && prior.view.payment.budget_id.as_deref() == Some(&budget_id),
                        "delivery offer or budget changed across retry"
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
                    view: view(&intent_id, role, &order, file_bytes)?,
                    order,
                    wallet_signature,
                    source_seals: Vec::new(),
                    grants: Vec::new(),
                    accepted: false,
                    notified: false,
                    allocation_batch: Vec::new(),
                    chain_observed_at_ms: 0,
                    settlement_complete: false,
                };
                stored.view.payment.budget_id = Some(budget_id);
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
                ensure!(
                    stored.view.mode == Mode::Paid,
                    "hold requires the paid protocol"
                );
                {
                    let chain = self.chain().await?;
                    let position = chain.confirmed_position().await?;
                    self.paid_state_at(&chain, &position, &stored.order).await?;
                    self.queue_hold(&chain, &stored, index, seal).await?;
                    if stored.view.windows[index as usize].state == WindowState::Pending {
                        stored.view.windows[index as usize].state = WindowState::Held;
                    }
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
                let budget_id = stored
                    .view
                    .payment
                    .budget_id
                    .as_deref()
                    .context("protected budget missing")?;
                let record = self.budget_record(budget_id)?;
                let observed = self
                    .chain()
                    .await?
                    .execute(
                        self,
                        &format!("budget-{budget_id}/open-{order_id}-{index}"),
                        chain::Call::Open {
                            order_id: parse_word(&order_id)?,
                            index,
                            seal,
                        },
                        &record.provider_gas_cap,
                    )
                    .await?;
                if observed.phase == "confirmed" {
                    self.update_paid_view(&mut stored).await?;
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
                ensure!(
                    stored.view.mode == Mode::Paid,
                    "order requires the paid protocol"
                );
                self.update_paid_view(&mut stored).await?;
                self.save(&mut stored)?;
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
        stored
            .view
            .windows
            .get(index as usize)
            .context("invalid delivery window")?;
        ensure!(
            stored.view.mode == Mode::Paid,
            "charity uses its independent object protocol"
        );
        {
            let state = self.paid_state(&stored.order).await?;
            let state = state
                .windows
                .get(index as usize)
                .context("chain window is missing")?;
            ensure!(state.phase != 4, "refunded service window is terminal");
            if matches!(operation, GrantOperation::GetCapsule) {
                ensure!(
                    state.phase == 2 || state.phase == 3,
                    "key capsule is unavailable until its publication is confirmed"
                );
            }
        }
        Ok(())
    }
    pub(super) async fn paid_state(&self, order: &Order) -> Result<chain::OrderState> {
        Ok(self.paid_state_with_budget(order).await?.0)
    }
    pub(super) async fn paid_state_with_budget(
        &self,
        order: &Order,
    ) -> Result<(chain::OrderState, budget::BudgetSnapshot)> {
        let chain = self.chain().await?;
        let position = chain.confirmed_position().await?;
        self.paid_state_at(&chain, &position, order).await
    }
    pub(super) async fn paid_state_at(
        &self,
        chain: &chain::Chain,
        position: &chain::ChainPosition,
        order: &Order,
    ) -> Result<(chain::OrderState, budget::BudgetSnapshot)> {
        ensure!(
            order.unsigned.terms.acceptance == vibeface_delivery::Acceptance::Recoverable,
            "protected delivery supports Recoverable only"
        );
        let id = order.unsigned.terms.order_id()?;
        let state = chain.order_at(position, id).await?;
        let budget_id = chain.order_budget_at(position, id).await?;
        ensure!(
            budget_id != [0; 32],
            "paid delivery has no irrevocable protected allocation"
        );
        let budget = chain.budget_at(position, budget_id.into()).await?;
        ensure!(
            state.windows.len() == order.unsigned.windows.len()
                && state.payer == chain::address_hex(order.unsigned.terms.payer.into())
                && state.provider_wallet
                    == chain::address_hex(order.unsigned.terms.provider_wallet.into())
                && budget.terms.payer == state.payer
                && budget.terms.payee == state.provider_wallet
                && budget.terms.provider_peer == hex::encode(order.unsigned.terms.provider_peer),
            "chain allocation differs from its signed manifest or fixed budget roles"
        );
        for window in &order.unsigned.windows {
            ensure!(
                chain
                    .public_values_at(position, id, window.index, Relation::Source)
                    .await?
                    == Statement::window(&order.unsigned, window.index, Relation::Source)?,
                "chain public values differ from the signed order"
            );
        }
        chain.verify_position(position).await?;
        Ok((state, budget))
    }
    pub(super) async fn run_provider(&self) -> Result<()> {
        if !self.config().paid_enabled {
            return Ok(());
        }
        let Ok(_provider) = self.provider_operations.try_lock() else {
            return Ok(());
        };
        let mut orders = self.with_store(|store| store.progress_orders(None))?;
        orders.retain(|order| order.view.role == Role::Provider && order.view.mode == Mode::Paid);
        orders.sort_by_key(|order| order.view.updated_at_ms);
        let transaction = self.with_store(|store| store.next_transaction_for_reconcile())?;
        if orders.is_empty() && transaction.is_none() {
            return Ok(());
        }
        let budget_id = orders
            .iter()
            .find(|order| order.accepted && !order.historical())
            .map(|first| {
                first
                    .view
                    .payment
                    .budget_id
                    .clone()
                    .context("protected provider order has no budget")
            })
            .transpose()?;
        // Persist rotation before any await can fail or be cancelled by the step deadline.
        for order in orders.iter_mut().filter(|order| {
            order.accepted && !order.historical() && order.view.payment.budget_id == budget_id
        }) {
            self.save(order)?;
        }
        let chain = self.chain().await?;
        if let Some(transaction) = transaction {
            let observed = if transaction.phase == "confirmed" {
                let observed = chain.observe(&transaction).await.unwrap_or_else(|_| chain::TxObservation {
                    phase: "unknown".into(), tx_hash: transaction.tx_hash.clone(),
                    block_number: transaction.block_number, block_hash: transaction.block_hash.clone(),
                    error: Some("historical transaction observation is unknown; original bytes were preserved".into()),
                });
                self.with_store(|store| {
                    store.observe_transaction(
                        &transaction.intent.id,
                        &observed.phase,
                        observed.block_number,
                        observed.block_hash.as_deref(),
                        observed.error.as_deref(),
                    )
                })?;
                observed
            } else {
                chain
                    .reconcile_transaction(self, &transaction.intent.id)
                    .await?
            };
            self.finish_key_operation(&chain, &transaction.intent.id, &observed.phase)
                .await?;
        }
        if let Some(budget_id) = budget_id {
            let mut operation_error = self
                .flush_budget_holds(&budget_id)
                .await
                .err()
                .map(|error| error.to_string());
            let result = async {
                let snapshot = self.budget_snapshot(&budget_id).await?;
                if snapshot.earned != snapshot.withdrawn {
                    let record = self.budget_record(&budget_id)?;
                    chain
                        .execute(
                            self,
                            &format!("budget-{budget_id}/withdraw-{}", snapshot.earned),
                            chain::Call::Budget(Box::new(budget::BudgetAction::Withdraw {
                                budget_id: parse_word(&budget_id)?,
                            })),
                            &record.provider_gas_cap,
                        )
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                operation_error = Some(error.to_string());
            }
            for mut order in orders
                .iter()
                .filter(|order| {
                    order.accepted
                        && !order.historical()
                        && order.view.payment.budget_id.as_deref() == Some(&budget_id)
                })
                .cloned()
            {
                match self.update_paid_view(&mut order).await {
                    Ok(()) => order.view.error = operation_error.clone(),
                    Err(error) => {
                        order.view.chain_unknown();
                        order.view.error = Some(error.to_string());
                    }
                }
                self.save(&mut order)?;
            }
        }
        if let Some(mut order) = orders
            .iter()
            .filter(|order| {
                order.historical()
                    && crate::runtime::now_ms().saturating_sub(order.chain_observed_at_ms) >= 60_000
            })
            .min_by_key(|order| order.chain_observed_at_ms)
            .cloned()
        {
            if let Err(error) = self.update_paid_view(&mut order).await {
                order.view.chain_unknown();
                order.view.error = Some(error.to_string());
            }
            self.save(&mut order)?;
            if order.historical() && now() > order.order.unsigned.terms.retain_until {
                self.with_store(|store| store.release_provider_objects(&order.view.order_id))?;
            }
        }
        if let Some(mut order) = orders
            .into_iter()
            .filter(|order| {
                !order.accepted
                    && now() > order.order.unsigned.terms.retain_until
                    && crate::runtime::now_ms().saturating_sub(order.chain_observed_at_ms) >= 60_000
            })
            .min_by_key(|order| order.chain_observed_at_ms)
        {
            match chain
                .unfunded_after(
                    order.order.unsigned.terms.order_id()?,
                    order.order.unsigned.terms.retain_until,
                )
                .await
            {
                Ok(true) => self.with_store(|store| store.reclaim_unfunded_quote(&order))?,
                Ok(false) => {
                    order.chain_observed_at_ms = crate::runtime::now_ms();
                    self.save(&mut order)?;
                }
                Err(error) => {
                    order.chain_observed_at_ms = crate::runtime::now_ms();
                    order.view.error = Some(format!(
                        "expired quotation funding remains unknown: {error}"
                    ));
                    self.save(&mut order)?;
                }
            }
        }
        Ok(())
    }
    pub(super) async fn update_paid_view(&self, stored: &mut StoredOrder) -> Result<()> {
        stored.settlement_complete = false;
        let (state, budget) = self.paid_state_with_budget(&stored.order).await?;
        ensure!(
            stored.view.payment.budget_id.as_deref() == Some(&budget.terms.budget_id),
            "stored order points to another protected budget"
        );
        if stored.view.role == Role::Receiver
            && stored.accepted
            && state.windows.iter().any(|window| window.phase != 4)
        {
            self.receiver_secret(stored.order.unsigned.terms.binding.key_id)?;
        }
        apply_paid_snapshot(&mut stored.view, &state)?;
        let publication = self.publication_status(&stored.view.order_id)?;
        if state.earned != "0" && budget.earned == budget.withdrawn {
            stored.view.payment.state = PaymentState::Withdrawn;
        }
        stored.view.payment.settlement = Some(BudgetSettlement {
            earned: budget.earned,
            withdrawn: budget.withdrawn,
            refunded: budget.refunded,
            publication,
        });
        stored.chain_observed_at_ms = crate::runtime::now_ms();
        stored.settlement_complete = budget::order_is_settled(&state, &stored.order)?
            && stored
                .view
                .payment
                .settlement
                .as_ref()
                .is_some_and(|budget| budget.earned == budget.withdrawn);
        Ok(())
    }
}

pub(super) fn apply_paid_snapshot(
    view: &mut DeliveryOrder,
    state: &chain::OrderState,
) -> Result<()> {
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
                budget_id: None,
                settlement: None,
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
    #[tokio::test]
    async fn disabled_paid_requests_make_no_chain_calls_or_new_store_and_keep_local_history()
    -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut chain = chain::tests::config();
        chain.rpc_url = format!("http://{}", listener.local_addr()?);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post({
                let calls = calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        axum::Json(json!({"error":"disabled requests must not reach RPC"}))
                    }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/delivery-disabled-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir)?;
        crate::config::Config::local("127.0.0.1:0".parse()?, vec![], false).save(&dir)?;
        crate::config::private_write(
            &dir.join("data-epoch.json"),
            &crate::config::data_epoch_marker(),
        )?;
        crate::config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let node = Runtime::open(&dir).await?;
        node.delivery.configure_local(DeliveryConfig {
            enabled: true,
            paid_enabled: false,
            chain: Some(chain),
            ..Default::default()
        })?;
        let id = "ab".repeat(32);
        for request in [
            ServiceRequest::Hold {
                order_id: id.clone(),
                index: 0,
                consent: vibeface_delivery::PeerSignature {
                    r: [0; 32],
                    s: [0; 32],
                },
                seal: Vec::new(),
            },
            ServiceRequest::Open {
                order_id: id.clone(),
                index: 0,
                seal: Vec::new(),
            },
            ServiceRequest::Order { order_id: id },
        ] {
            let error = node
                .delivery
                .handle_service(&node, &"11".repeat(32), request)
                .await
                .err()
                .context("paid request must be disabled")?;
            assert!(error.to_string().contains("disabled"));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(!dir.join("delivery/service.sqlite3").exists());
        let order = super::super::store::tests::paid_order(Role::Sender);
        node.delivery.with_store(|store| store.save_order(&order))?;
        let history = node
            .delivery
            .client_request(
                &node,
                "owner-network",
                "owner",
                json!({"op":"get","order_id":order.view.order_id}),
            )
            .await?;
        assert_eq!(history["order_id"], order.view.order_id);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        node.shutdown().await;
        drop(node);
        server.abort();
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn paid_and_refunded_history_requires_complete_money_and_receiver_recovery() {
        let mut order = super::super::store::tests::paid_order(Role::Receiver);
        order.accepted = true;
        order.view = view();
        order.settlement_complete = true;
        order.view.payment.budget_id = Some("aa".repeat(32));
        order.view.payment.settlement = Some(BudgetSettlement {
            earned: "29".into(),
            withdrawn: "29".into(),
            refunded: "0".into(),
            publication: None,
        });
        assert!(
            !order.historical(),
            "Paid does not prove local file recovery"
        );
        order.view.name = Some("original.bin".into());
        order.view.sha256 = Some("11".repeat(32));
        assert!(order.historical());
        order.view.windows[0].decryption_proven = false;
        order.view.evidence = Evidence::Recoverable;
        order.view.payment.withdrawn = "0".into();
        assert!(
            order.historical(),
            "actual Recoverable recovery needs no automatic Open or per-order withdrawal"
        );
        order.view.evidence = Evidence::None;
        assert!(
            !order.historical(),
            "metadata alone is not current recovery evidence"
        );
        order.view.evidence = Evidence::Recoverable;
        order.view.payment.settlement.as_mut().unwrap().withdrawn = "0".into();
        assert!(
            !order.historical(),
            "unwithdrawn budget income remains active"
        );
        order.view.payment.settlement.as_mut().unwrap().withdrawn = "29".into();
        order.settlement_complete = false;
        assert!(
            !order.historical(),
            "pending bonus settlement remains active"
        );
        order.settlement_complete = true;
        order.view.chain_unknown();
        assert!(
            !order.historical(),
            "Unknown and reorg facts retract completion"
        );
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
