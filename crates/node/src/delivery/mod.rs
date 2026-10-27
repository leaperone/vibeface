pub mod chain;
mod charity;
mod keys;
mod orders;
pub use keys::KeyAction;
pub mod proof;
mod provider;
mod service;
pub mod store;
pub mod types;
pub mod wallet;

use crate::{crypto::LocalIdentity, runtime::Runtime};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use store::ServiceStore;
use types::*;
use vibeface_delivery::{
    Amount, BindingAction, KeyBinding, Order, ProofInput, ProofProfile, Relation, Secret32,
    SecretBytes, SignedKeyBinding, Statement, Terms, UnsignedOrder, WindowRandomness, Witness,
    Word, encrypt_window, hash, receiving_public, recover_window,
};
use zeroize::Zeroizing;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryConfig {
    pub enabled: bool,
    pub charity_enabled: bool,
    pub paid_enabled: bool,
    pub storage_limit_bytes: u64,
    pub max_orders: u32,
    pub chain: Option<chain::ChainConfig>,
    pub prover: Option<proof::ProofConfig>,
    pub delivery_fee_per_window: String,
    pub reservation_fee_per_window: String,
    pub priority_fee: String,
    pub provider_gas_cap: String,
}
impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            charity_enabled: true,
            paid_enabled: false,
            storage_limit_bytes: 64 * 1024 * 1024,
            max_orders: 128,
            chain: None,
            prover: None,
            delivery_fee_per_window: "10".into(),
            reservation_fee_per_window: "2".into(),
            priority_fee: "5".into(),
            provider_gas_cap: "0".into(),
        }
    }
}
impl DeliveryConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (131_208..=1024 * 1024 * 1024).contains(&self.storage_limit_bytes)
                && (1..=128).contains(&self.max_orders),
            "invalid delivery capacity"
        );
        for value in [
            &self.delivery_fee_per_window,
            &self.reservation_fee_per_window,
            &self.priority_fee,
            &self.provider_gas_cap,
        ] {
            Amount::from_decimal_str(value)?;
        }
        if let Some(chain) = &self.chain {
            chain.validate()?;
        }
        if let Some(prover) = &self.prover {
            prover.validate()?;
        }
        if self.paid_enabled {
            let chain = self
                .chain
                .as_ref()
                .context("paid delivery requires a pinned local deployment")?;
            if let Some(prover) = &self.prover {
                ensure!(
                    chain.profile == prover.profile,
                    "chain and local proof profiles differ"
                );
            }
            ensure!(
                chain.profile.max_windows <= 2,
                "Node delivery only exposes the two-window experimental profile"
            );
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct ReceivingMaterial {
    binding: SignedKeyBinding,
    secret: Option<Secret32>,
}

pub struct Delivery {
    data_dir: PathBuf,
    identity: LocalIdentity,
    settings: Mutex<DeliveryConfig>,
    store: Mutex<Option<ServiceStore>>,
    pub(crate) wallet: Mutex<wallet::Wallet>,
    proofs: proof::ProofRunner,
    service_slots: tokio::sync::Semaphore,
    peer_slots: Mutex<HashMap<String, Weak<tokio::sync::Semaphore>>>,
    operations: tokio::sync::Mutex<()>,
    provider_operations: tokio::sync::Mutex<()>,
    runtime: std::sync::OnceLock<Weak<Runtime>>,
    wakeup: Arc<tokio::sync::Notify>,
    changes: tokio::sync::watch::Sender<u64>,
    executor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Delivery {
    pub fn open(dir: &Path, identity: LocalIdentity) -> Result<Self> {
        let settings = dir.join("delivery-settings.json");
        let config = if settings.try_exists()? {
            serde_json::from_slice(&std::fs::read(settings)?)?
        } else {
            DeliveryConfig::default()
        };
        config.validate()?;
        Ok(Self {
            data_dir: dir.to_owned(),
            identity,
            settings: Mutex::new(config),
            store: Mutex::new(None),
            wallet: Mutex::new(wallet::Wallet::open(dir.join("delivery-wallet.json"))?),
            proofs: Default::default(),
            service_slots: tokio::sync::Semaphore::new(16),
            peer_slots: Mutex::new(HashMap::new()),
            operations: tokio::sync::Mutex::new(()),
            provider_operations: tokio::sync::Mutex::new(()),
            runtime: std::sync::OnceLock::new(),
            wakeup: Arc::new(tokio::sync::Notify::new()),
            changes: tokio::sync::watch::Sender::new(0),
            executor: Mutex::new(None),
        })
    }
    pub fn config(&self) -> DeliveryConfig {
        self.settings.lock().expect("delivery settings").clone()
    }
    pub(crate) fn attach(&self, runtime: Weak<Runtime>) -> Result<()> {
        ensure!(
            self.runtime.set(runtime).is_ok(),
            "delivery runtime already attached"
        );
        let runtime = self
            .runtime
            .get()
            .context("delivery runtime is not attached")?
            .clone();
        let wakeup = self.wakeup.clone();
        let executor = tokio::spawn(async move {
            loop {
                tokio::select! {_=wakeup.notified()=>{},_=tokio::time::sleep(std::time::Duration::from_secs(5))=>{}}
                let Some(node) = runtime.upgrade() else {
                    break;
                };
                let mut changes = node.delivery.changes.subscribe();
                let mut membership = node.membership.subscribe();
                tokio::select! {
                    result=node.delivery.run_authorized(&node)=>{
                        if let Err(error)=result {tracing::debug!(error=%error,"delivery executor waits for reconciliation");}
                    },
                    _=changes.changed()=>{},
                    _=membership.changed()=>{node.delivery.wakeup.notify_one();},
                }
            }
        });
        *self.executor.lock().expect("delivery executor") = Some(executor);
        self.wakeup.notify_one();
        Ok(())
    }
    async fn run_authorized(&self, node: &Runtime) -> Result<()> {
        if !self.config().enabled {
            return Ok(());
        }
        let Ok(_operation) = self.operations.try_lock() else {
            return Ok(());
        };
        let _maintenance = node.maintenance.enter()?;
        match tokio::time::timeout(std::time::Duration::from_secs(30), self.run_provider()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::debug!(error=%error,"paid provider reconciliation is pending")
            }
            Err(_) => tracing::debug!("paid provider reconciliation reached its step deadline"),
        }
        if node.has_membership().await? {
            let network = node.membership_state().await?.genesis.network_id;
            let mut orders = self.with_store(|store| store.orders(Some(&network)))?;
            orders.sort_by_key(|order| order.view.updated_at_ms);
            for order in orders {
                if !(order.confirmed || order.accepted) {
                    continue;
                }
                if order.view.phase == Phase::Failed && !order.resume_requested {
                    continue;
                }
                if order.view.error.is_some() && !order.resume_requested {
                    continue;
                }
                let id = order.view.order_id.clone();
                let result = if order.view.role == Role::Sender
                    && matches!(
                        order.view.phase,
                        Phase::Prepared
                            | Phase::Proving
                            | Phase::FundingPending
                            | Phase::Reconciling
                    ) {
                    self.confirm(node, order).await
                } else {
                    self.reconcile(node, order).await
                };
                match result {
                    Ok(mut order) => {
                        order.resume_requested = false;
                        order.view.error = None;
                        self.save(&mut order)?;
                    }
                    Err(error) => {
                        let mut order = self.with_store(|store| store.order(&id))?;
                        order.resume_requested = error.is::<proof::Busy>();
                        order.view.error = Some(error.to_string());
                        if order.view.mode == Mode::Paid
                            && !matches!(order.view.phase, Phase::Prepared | Phase::Proving)
                        {
                            order.view.chain_unknown();
                        }
                        self.save(&mut order)?;
                    }
                }
                break;
            }
            self.charity_run_authorized(node).await?;
        }
        Ok(())
    }
    pub(crate) async fn shutdown(&self) {
        let executor = self.executor.lock().expect("delivery executor").take();
        if let Some(executor) = executor {
            executor.abort();
            let _ = executor.await;
        }
    }
    pub(crate) async fn authorize_spend(&self, intent: &str) -> Result<()> {
        self.ensure_mode(Mode::Paid)?;
        let group = intent
            .split('/')
            .next()
            .context("spend intent has no order")?;
        if let Some(serial) = group.strip_prefix("key-") {
            ensure!(
                !serial.is_empty()
                    && self
                        .with_store(|store| store.value(&format!("local-spend:{group}")))?
                        .is_some(),
                "key registration requires a local owner intent"
            );
            return Ok(());
        }
        let order = self.with_store(|store| store.order(group))?;
        if order.view.role == Role::Provider && order.network_id.is_none() {
            return Ok(());
        }
        let actor = order
            .authorized_by
            .context("delivery spend has no managing member authorization")?;
        self.authorize_effect(
            order
                .network_id
                .as_deref()
                .context("delivery spend has no network")?,
            &actor,
        )
        .await
    }
    pub(super) async fn authorize_effect(&self, network: &str, actor: &str) -> Result<()> {
        let runtime = self
            .runtime
            .get()
            .and_then(Weak::upgrade)
            .context("delivery runtime stopped")?;
        let view = runtime.membership_state().await?.project()?;
        ensure!(
            network == view.network_id && view.client(actor)?.manage,
            "delivery managing member was revoked"
        );
        view.node(&runtime.identity.device_id()?)?;
        ensure!(
            *runtime.write_state.borrow() == crate::store::WriteState::Writable,
            "delivery spending is paused until this Node is writable"
        );
        Ok(())
    }
    pub fn with_store<T>(&self, run: impl FnOnce(&mut ServiceStore) -> Result<T>) -> Result<T> {
        let config = self.config();
        let mut store = self.store.lock().expect("delivery store");
        if store.is_none() {
            ensure!(
                config.enabled || self.data_dir.join("delivery/service.sqlite3").exists(),
                "delivery service is disabled"
            );
            *store = Some(ServiceStore::open(
                &self.data_dir,
                config.storage_limit_bytes,
                config.max_orders,
            )?);
        }
        run(store.as_mut().context("delivery store unavailable")?)
    }
    pub fn configure_local(&self, config: DeliveryConfig) -> Result<()> {
        config.validate()?;
        let mut store = self.store.lock().expect("delivery store");
        if let Some(store) = store.as_ref() {
            store.check_limits(config.storage_limit_bytes, config.max_orders)?;
        }
        crate::config::private_write(
            &self.data_dir.join("delivery-settings.json"),
            &serde_json::to_vec_pretty(&config)?,
        )?;
        if let Some(store) = store.as_mut() {
            store.set_limits(config.storage_limit_bytes, config.max_orders);
        }
        *self.settings.lock().expect("delivery settings") = config;
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        self.wakeup.notify_one();
        Ok(())
    }
    pub fn status(&self) -> Result<Value> {
        let config = self.config();
        let key = if config.enabled && config.charity_enabled {
            self.charity_key_status()?
        } else if config.enabled && config.paid_enabled {
            let key = self.key_binding(Mode::Paid)?.binding;
            if key.action == BindingAction::Active {
                json!({"key_id":hex::encode(key.key_id),"serial":key.serial,"public_key":hex::encode(key.receiver_public)})
            } else {
                Value::Null
            }
        } else {
            Value::Null
        };
        let mut wallet =
            serde_json::to_value(self.wallet.lock().expect("delivery wallet").status())?;
        if wallet.get("address").is_none() {
            wallet["address"] = Value::Null;
        }
        Ok(
            json!({"enabled":config.enabled,"charity_enabled":config.charity_enabled,"paid_enabled":config.paid_enabled,
            "node_peer":self.identity.device_id()?,"wallet":wallet,
            "max_file_bytes":MAX_FILE_BYTES,"receiver_key":key}),
        )
    }
    fn key_binding(&self, mode: Mode) -> Result<SignedKeyBinding> {
        self.ensure_mode(mode)?;
        ensure!(
            mode == Mode::Paid,
            "charity has an independent receiving key"
        );
        let label = keys::current_key_label(&self.config())?;
        self.with_store(|store| {
            if let Some(bytes) = store.value(&label)? {
                let bytes = Zeroizing::new(bytes);
                let material: ReceivingMaterial = serde_json::from_slice(&bytes)?;
                return Ok(material.binding);
            }
            let material = self.new_receiving_material(1, BindingAction::Active)?;
            let key = format!(
                "receiving-key:{}",
                hex::encode(material.binding.binding.key_id)
            );
            let encoded = Zeroizing::new(serde_json::to_vec(&material)?);
            store.put_values(&[
                (label.as_str(), encoded.as_slice()),
                (key.as_str(), encoded.as_slice()),
            ])?;
            Ok(material.binding)
        })
    }
    fn receiver_secret(&self, key_id: Word) -> Result<Secret32> {
        let bytes = Zeroizing::new(
            self.with_store(|store| {
                store.value(&format!("receiving-key:{}", hex::encode(key_id)))
            })?
            .context("receiving key is unavailable; restore the independent delivery backup")?,
        );
        let material: ReceivingMaterial = serde_json::from_slice(&bytes)?;
        material
            .secret
            .context("receiving key material has been revoked")
    }
    fn ensure_mode(&self, mode: Mode) -> Result<()> {
        let config = self.config();
        ensure!(config.enabled, "object delivery is disabled");
        ensure!(
            match mode {
                Mode::Charity => config.charity_enabled,
                Mode::Paid => config.paid_enabled,
            },
            "requested delivery mode is disabled"
        );
        Ok(())
    }
    async fn chain(&self) -> Result<chain::Chain> {
        chain::Chain::connect(
            self.config()
                .chain
                .context("paid deployment is not configured")?,
        )
        .await
    }
    fn scoped_order(&self, network: &str, id: &str) -> Result<StoredOrder> {
        let order = self.with_store(|store| store.order(id))?;
        ensure!(
            order.network_id.as_deref() == Some(network),
            "delivery order belongs to another personal network"
        );
        Ok(order)
    }
    pub(crate) async fn client_request(
        &self,
        node: &Runtime,
        network: &str,
        device: &str,
        params: Value,
    ) -> Result<Value> {
        let op = params["op"]
            .as_str()
            .context("delivery operation is required")?;
        match op {
            "status" => self.status(),
            "find_intent" => {
                let intent = text(&params, "intent_id")?;
                uuid::Uuid::parse_str(intent)?;
                if !self.config().enabled
                    && !self.data_dir.join("delivery/service.sqlite3").exists()
                {
                    return Ok(json!({"order":null}));
                }
                let order = self
                    .with_store(|store| store.by_intent(network, intent))?
                    .map(|order| order.view);
                let order = match order {
                    Some(order) => Some(order),
                    None => self.charity_find_intent(network, intent)?,
                };
                Ok(json!({"order":order}))
            }
            "configure" => {
                let mut config = self.config();
                config.enabled = params["enabled"]
                    .as_bool()
                    .context("enabled must be a boolean")?;
                config.charity_enabled = params["charity_enabled"]
                    .as_bool()
                    .context("charity_enabled must be a boolean")?;
                config.paid_enabled = params["paid_enabled"]
                    .as_bool()
                    .context("paid_enabled must be a boolean")?;
                self.configure_local(config)?;
                self.status()
            }
            "list" => {
                if !self.config().enabled
                    && !self.data_dir.join("delivery/service.sqlite3").exists()
                {
                    return Ok(json!({"orders":[]}));
                }
                let mut orders = self
                    .with_store(|store| store.orders(Some(network)))?
                    .into_iter()
                    .map(|o| o.view)
                    .collect::<Vec<_>>();
                orders.extend(self.charity_list(network)?);
                orders.sort_by_key(|order| std::cmp::Reverse(order.created_at_ms));
                Ok(json!({"orders":orders}))
            }
            "get" => {
                let id = text(&params, "order_id")?;
                if self.with_store(|store| store.charity_order(id))?.is_some() {
                    return Ok(serde_json::to_value(self.charity_get(network, id)?)?);
                }
                Ok(serde_json::to_value(self.scoped_order(network, id)?.view)?)
            }
            "prepare" => {
                let _operation = self
                    .operations
                    .try_lock()
                    .context("another delivery operation is in progress")?;
                let mut value = params;
                value
                    .as_object_mut()
                    .context("invalid delivery request")?
                    .remove("op");
                let request: Prepare = serde_json::from_value(value)?;
                if request.mode == Mode::Charity {
                    return Ok(serde_json::to_value(
                        self.charity_prepare(node, network, request).await?,
                    )?);
                }
                let mut order = self.prepare(node, network, request).await?;
                if order.authorized_by.is_none() {
                    order.authorized_by = Some(device.into());
                    self.save(&mut order)?;
                }
                Ok(serde_json::to_value(order.view)?)
            }
            "confirm" => {
                let intent = text(&params, "intent_id")?;
                let operation = self.operations.try_lock();
                if operation.is_err() {
                    if let Some(order) =
                        self.with_store(|store| store.by_intent(network, intent))?
                        && order.confirmed
                    {
                        return Ok(serde_json::to_value(order.view)?);
                    }
                    if let Some(order) = self.charity_find_intent(network, intent)?
                        && order.phase != Phase::Prepared
                    {
                        return Ok(serde_json::to_value(order)?);
                    }
                }
                let _operation = operation.context("another delivery operation is in progress")?;
                let Some(mut order) = self.with_store(|store| store.by_intent(network, intent))?
                else {
                    return Ok(serde_json::to_value(
                        self.charity_confirm(node, network, intent, device).await?,
                    )?);
                };
                if !order.confirmed {
                    ensure!(
                        !self
                            .with_store(|store| store.orders(Some(network)))?
                            .iter()
                            .any(|other| other.view.order_id != order.view.order_id
                                && other.view.role == Role::Sender
                                && other.confirmed
                                && !matches!(
                                    other.view.phase,
                                    Phase::Paid | Phase::Refunded | Phase::Failed
                                )),
                        "finish or refund the current paid order before confirming another"
                    );
                }
                order.authorized_by = Some(device.into());
                ensure!(
                    matches!(
                        self.wallet.lock().expect("delivery wallet").status(),
                        wallet::WalletStatus::Unlocked { .. }
                    ),
                    "unlock the independent delivery wallet locally before confirming payment"
                );
                order.confirmed = true;
                order.resume_requested = true;
                if order.view.phase == Phase::Prepared {
                    order.view.phase = Phase::FundingPending;
                }
                self.save(&mut order)?;
                self.wakeup.notify_one();
                Ok(serde_json::to_value(order.view)?)
            }
            "accept" | "reconcile" => {
                let _operation = self
                    .operations
                    .try_lock()
                    .context("another delivery operation is in progress")?;
                let id = text(&params, "order_id")?;
                if self.with_store(|store| store.charity_order(id))?.is_some() {
                    return Ok(serde_json::to_value(
                        self.charity_action(node, network, id, op == "accept", device)
                            .await?,
                    )?);
                }
                let mut order = self.scoped_order(network, id)?;
                if op == "accept" {
                    ensure!(
                        order.view.role == Role::Receiver,
                        "only the receiving Node can accept delivery"
                    );
                    if !order.accepted {
                        ensure!(
                            !self
                                .with_store(|store| store.orders(Some(network)))?
                                .iter()
                                .any(|other| other.view.order_id != order.view.order_id
                                    && other.view.role == Role::Receiver
                                    && other.accepted
                                    && !matches!(
                                        other.view.phase,
                                        Phase::Paid | Phase::Refunded | Phase::Failed
                                    )),
                            "finish the accepted paid order before accepting another"
                        );
                    }
                    order.accepted = true;
                    order.authorized_by = Some(device.into());
                    self.save(&mut order)?;
                }
                order.resume_requested = true;
                order.view.error = None;
                self.save(&mut order)?;
                self.wakeup.notify_one();
                Ok(serde_json::to_value(order.view)?)
            }
            "read" => {
                let id = text(&params, "order_id")?;
                if self.with_store(|store| store.charity_order(id))?.is_some() {
                    return self.charity_read(network, id);
                }
                let order = self.scoped_order(network, id)?;
                ensure!(
                    order.view.role == Role::Receiver && order.accepted,
                    "delivery must be accepted on the receiving Node"
                );
                self.read(&order)
            }
            _ => bail!("unknown delivery operation"),
        }
    }
    fn save(&self, order: &mut StoredOrder) -> Result<()> {
        order.view.updated_at_ms = crate::runtime::now_ms();
        self.with_store(|store| store.save_order(order))
    }
}

fn now() -> u64 {
    (crate::runtime::now_ms().max(0) as u64) / 1000
}
fn random_word() -> Word {
    rand::random()
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("{key} must be a string"))
}
fn deployment(config: &DeliveryConfig, mode: Mode) -> Result<(Word, [u8; 20])> {
    ensure!(mode == Mode::Paid, "charity has no chain deployment");
    let chain = config
        .chain
        .as_ref()
        .context("paid deployment is missing")?;
    let mut id = [0; 32];
    id[24..].copy_from_slice(&chain.chain_id.to_be_bytes());
    let escrow = hex::decode(chain.escrow.trim_start_matches("0x"))?
        .try_into()
        .ok()
        .context("invalid escrow")?;
    Ok((id, escrow))
}
fn paid_deadlines(at: u64, profile: &ProofProfile) -> Deadlines {
    let budget = u64::from(profile.max_windows)
        * (proof::INFO_TIMEOUT_SECS + proof::PROVE_TIMEOUT_SECS + proof::VERIFY_TIMEOUT_SECS)
        + 600;
    let quote_expires_at = at + budget;
    let hold_deadline = quote_expires_at + budget;
    let key_deadline = hold_deadline + 600;
    let open_deadline = key_deadline + budget;
    Deadlines {
        quote_expires_at,
        hold_deadline,
        key_deadline,
        open_deadline,
        retain_until: open_deadline + 86400,
        priority_deadline: quote_expires_at + 3600,
    }
}
