use super::*;
use std::path::Path;
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TransferTerms {
    receiver: String,
    provider: String,
    mode: String,
    acceptance: String,
    name: String,
    bytes: u64,
    sha256: String,
    service_cap: String,
    gas_cap: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_id: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransferIntent {
    intent_id: String,
    network_id: String,
    client_id: String,
    target_node: String,
    terms: TransferTerms,
}
impl Backend {
    pub(super) async fn refresh_delivery(&mut self) -> Result<()> {
        self.start_delivery_refresh().await
    }
    pub(super) async fn apply_delivery_refresh(
        &mut self,
        status: Value,
        list: Value,
        providers: Value,
        budgets: Option<Result<Value>>,
    ) -> Result<()> {
        let mut delivery: Delivery = serde_json::from_value(status)?;
        let target = self
            .active_entry
            .as_ref()
            .context("当前设备未确认")?
            .node_id
            .as_str();
        ensure!(delivery.node_peer == target, "文件传输状态不属于当前设备");
        delivery.orders = decode_list(&list, "orders")?;
        for order in &delivery.orders {
            validate_transfer_order(order)?;
            ensure!(
                match order.role.as_str() {
                    "sender" => order.source_peer == target,
                    "receiver" => order.receiver_peer == target,
                    "provider" => order.provider_peer == target,
                    _ => false,
                },
                "订单不属于当前设备角色"
            );
            self.validate_quote(order).await?;
        }
        delivery.providers = decode_list(&providers, "providers")?;
        match budgets {
            Some(Ok(value)) => match parse_transfer_budgets(value) {
                Ok(budgets) => {
                    delivery.budgets = budgets;
                    delivery.budget_error = None;
                }
                Err(error) => {
                    delivery.budgets.clear();
                    delivery.budget_error =
                        Some(format!("预算数据未通过校验，请刷新后核对：{error}"));
                }
            },
            Some(Err(error)) => {
                delivery.budgets.clear();
                delivery.budget_error = Some(format!("预算状态未读回，请刷新后核对：{error}"));
            }
            None => {
                delivery.budgets.clear();
                delivery.budget_error = None;
            }
        }
        self.state.delivery = delivery;
        self.publish();
        Ok(())
    }
    pub(super) async fn delivery_intent(
        &mut self,
        terms: Value,
        restart: Option<TransferOrder>,
    ) -> Result<String> {
        let client = self.client()?;
        let terms: TransferTerms = serde_json::from_value(terms)?;
        let target = self
            .active_entry
            .as_ref()
            .context("当前设备未确认")?
            .node_id
            .clone();
        let network = client.network_id().to_owned();
        let identity = client.client_id().to_owned();
        let original = if let Some(review) = restart {
            let current: TransferOrder = serde_json::from_value(
                self.client()?
                    .read(ReadRequest::Delivery(DeliveryRead::Get {
                        order_id: review.order_id.clone(),
                    }))
                    .await?,
            )?;
            validate_reviewed_terms(&review, &current)?;
            ensure!(
                can_restart_transfer(&current),
                "原发送尚未明确结束，不会创建另一笔意图"
            );
            Some(current)
        } else {
            None
        };
        let path = self.client_directory()?.join("delivery-intents.json");
        remember_transfer_intent(path, network, identity, target, terms, original).await
    }
    pub(super) async fn open_delivery_restart(&mut self, original: TransferOrder) -> Result<()> {
        ensure!(
            self.state.can_manage && self.state.online,
            "重新发送需要当前管理连接"
        );
        let current: TransferOrder = serde_json::from_value(
            self.client()?
                .read(ReadRequest::Delivery(DeliveryRead::Get {
                    order_id: original.order_id.clone(),
                }))
                .await?,
        )?;
        validate_reviewed_terms(&original, &current)?;
        ensure!(
            can_restart_transfer(&current),
            "原发送尚未明确结束，请先查询原订单"
        );
        let entries = read_intents(self.client_directory()?.join("delivery-intents.json")).await?;
        let intent = entries
            .values()
            .find(|intent| intent.intent_id == current.intent_id)
            .context("原发送意图已被替换或不在此档案中，请先查看已有的新订单")?;
        validate_restart_intent(
            intent,
            &current,
            self.client()?.network_id(),
            self.client()?.client_id(),
            self.client()?.target_node(),
        )?;
        let terms = &intent.terms;
        let review = serde_json::to_string(&current)?;
        let mut values = BTreeMap::from([
            ("receiver_peer", terms.receiver.clone()),
            ("provider_peer", terms.provider.clone()),
            ("mode", terms.mode.clone()),
            ("acceptance", terms.acceptance.clone()),
            ("max_service_amount", terms.service_cap.clone()),
            ("max_gas_amount", terms.gas_cap.clone()),
            ("_restart_order_id", current.order_id),
            ("_restart_intent_id", current.intent_id),
            ("_restart_review", review),
        ]);
        if let Some(budget) = &terms.budget_id {
            values.insert("budget_id", budget.clone());
        }
        self.emit(Event::EditForm {
            editor: Editor::Transfer,
            values,
        })
        .await?;
        Ok(())
    }
    pub(super) async fn fresh_delivery_budgets(&self) -> Result<Vec<TransferBudget>> {
        parse_transfer_budgets(
            self.client()?
                .read(ReadRequest::Delivery(DeliveryRead::Budgets))
                .await?,
        )
    }
    pub(super) async fn prepare_budget(
        &self,
        id: &str,
        provider: &str,
        service_cap: &str,
        gas_cap: &str,
    ) -> Result<TransferBudget> {
        let status: Delivery = serde_json::from_value(
            self.client()?
                .read(ReadRequest::Delivery(DeliveryRead::Status))
                .await?,
        )?;
        ensure!(
            status.node_peer == self.client()?.target_node()
                && status.enabled
                && status.paid_enabled,
            "当前设备未确认启用受保护付费传输"
        );
        let payer = status
            .wallet
            .address
            .as_deref()
            .context("请在发送设备通过现有CLI建立预算和管理钱包")?;
        let budget = self
            .fresh_delivery_budgets()
            .await?
            .into_iter()
            .find(|budget| budget.budget_id == id)
            .context("原预算未读回，请重新选择已有开放预算")?;
        ensure!(
            budget.state == "open"
                && budget.provider_peer == provider
                && budget.payer.eq_ignore_ascii_case(payer)
                && budget.gas_cap == gas_cap
                && transfer_amount_at_most(service_cap, &budget.free),
            "预算角色、开放状态、可用金额或累计链费授权不一致"
        );
        let providers: Vec<TransferProvider> = decode_list(
            &self
                .client()?
                .read(ReadRequest::Delivery(DeliveryRead::Providers))
                .await?,
            "providers",
        )?;
        let offer = providers
            .iter()
            .find(|candidate| candidate.device_id == provider && candidate.paid)
            .and_then(|provider| provider.offer.as_ref())
            .and_then(|offer| offer.settlement.as_ref())
            .context("尚未读回此中转设备的当前付费结算配置")?;
        ensure!(
            offer.asset.eq_ignore_ascii_case(&budget.asset),
            "预算资产与当前中转设备报价资产不同"
        );
        Ok(budget)
    }
    pub(super) async fn check_order_budget(&self, order: &TransferOrder) -> Result<TransferBudget> {
        let id = order
            .payment
            .budget_id
            .as_deref()
            .context("受保护订单没有原预算")?;
        let budget = self
            .fresh_delivery_budgets()
            .await?
            .into_iter()
            .find(|budget| budget.budget_id == id)
            .context("原预算未读回，请刷新后核对")?;
        validate_order_budget(order, &budget)?;
        Ok(budget)
    }
    pub(super) async fn validate_quote(&self, order: &TransferOrder) -> Result<()> {
        if order.role != "sender" {
            return Ok(());
        }
        let client = self.client()?;
        let entries = read_intents(self.client_directory()?.join("delivery-intents.json")).await?;
        let Some(intent) = entries
            .values()
            .find(|intent| intent.intent_id == order.intent_id)
        else {
            return Ok(());
        };
        let terms = &intent.terms;
        ensure!(
            intent.network_id == client.network_id()
                && intent.client_id == client.client_id()
                && intent.target_node == order.source_peer,
            "订单与原本机付款意图身份不一致"
        );
        ensure!(
            order.receiver_peer == terms.receiver
                && order.provider_peer == terms.provider
                && order.mode == terms.mode
                && order.acceptance == terms.acceptance
                && order.name.as_deref() == Some(terms.name.as_str())
                && order.bytes == terms.bytes
                && order.sha256.as_deref() == Some(terms.sha256.as_str()),
            "订单与原文件或条款不一致，尚未确认付款"
        );
        ensure!(
            order.payment.budget_id == terms.budget_id,
            "订单预算与原本机发送意图不同，未确认付款"
        );
        ensure!(
            amount_at_most(&order.payment.total, &terms.service_cap)
                && amount_at_most(&order.payment.gas_cap, &terms.gas_cap),
            "订单费用超过原最高金额，尚未确认付款"
        );
        Ok(())
    }
}
fn read_intents_now(path: &Path) -> Result<BTreeMap<String, TransferIntent>> {
    if !path.try_exists()? {
        return Ok(BTreeMap::new());
    }
    let bytes = vibeface_bridge::platform::privatefs::read_private_bounded(
        path,
        4 * 1024 * 1024,
        "文件传输意图",
    )?;
    Ok(serde_json::from_slice(&bytes)?)
}
async fn read_intents(path: PathBuf) -> Result<BTreeMap<String, TransferIntent>> {
    tokio::task::spawn_blocking(move || read_intents_now(&path)).await?
}

pub(super) fn can_restart_transfer(order: &TransferOrder) -> bool {
    if order.role != "sender"
        || order.acceptance != "recoverable"
        || validate_transfer_order(order).is_err()
    {
        return false;
    }
    if order.mode == "charity" {
        return order.phase == "recoverable"
            && order.evidence == "charity_receipt"
            && !order.windows.is_empty()
            && order.windows.iter().all(|window| {
                matches!(window.state.as_str(), "held" | "key_published" | "earned")
            });
    }
    order.phase == "paid"
        && matches!(
            order.payment.state.as_str(),
            "earned" | "withdrawn" | "confirmed"
        )
        && !order.windows.is_empty()
        && order.windows.iter().all(|window| window.state == "earned")
        && transfer_sum([
            order.payment.earned.as_str(),
            order.payment.refunded.as_str(),
        ])
        .is_ok_and(|settled| settled == order.payment.total)
        || order.phase == "refunded"
            && order.payment.state == "refunded"
            && order.payment.refunded == order.payment.total
}

fn validate_restart_intent(
    intent: &TransferIntent,
    order: &TransferOrder,
    network: &str,
    client: &str,
    target: &str,
) -> Result<()> {
    ensure!(
        can_restart_transfer(order),
        "原发送尚未明确结束，不会创建另一笔意图"
    );
    ensure!(
        intent.intent_id == order.intent_id
            && intent.network_id == network
            && intent.client_id == client
            && intent.target_node == target
            && order.source_peer == target,
        "原发送不属于当前网络、身份或目标设备"
    );
    let terms = &intent.terms;
    ensure!(
        terms.receiver == order.receiver_peer
            && terms.provider == order.provider_peer
            && terms.mode == order.mode
            && terms.acceptance == order.acceptance
            && order.name.as_deref() == Some(terms.name.as_str())
            && order.bytes == terms.bytes
            && order.sha256.as_deref() == Some(terms.sha256.as_str())
            && terms.budget_id == order.payment.budget_id
            && amount_at_most(&order.payment.total, &terms.service_cap)
            && amount_at_most(&order.payment.gas_cap, &terms.gas_cap),
        "原文件、预算或最高金额与本机发送条款不一致"
    );
    Ok(())
}
fn amount_at_most(amount: &str, cap: &str) -> bool {
    if amount.is_empty()
        || cap.is_empty()
        || !amount
            .bytes()
            .chain(cap.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let amount = amount.trim_start_matches('0');
    let cap = cap.trim_start_matches('0');
    amount.len() < cap.len() || amount.len() == cap.len() && amount <= cap
}

async fn remember_transfer_intent(
    path: PathBuf,
    network: String,
    client: String,
    target: String,
    terms: TransferTerms,
    restart: Option<TransferOrder>,
) -> Result<String> {
    // The single Backend actor owns this profile. Compare and replace in one storage
    // operation so no await separates its existing-ID check from the atomic private write.
    tokio::task::spawn_blocking(move || {
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(
                &json!({"network":network,"client":client,"target":target,"terms":terms})
            )?)
        );
        let mut entries = read_intents_now(&path)?;
        if let Some(intent) = entries.get(&fingerprint) {
            ensure!(
                intent.network_id == network
                    && intent.client_id == client
                    && intent.target_node == target
                    && intent.terms == terms,
                "本机发送记录与原预算或条款不一致，未创建第二笔意图"
            );
            if let Some(original) = &restart {
                validate_restart_intent(intent, original, &network, &client, &target)?;
            } else {
                return Ok(intent.intent_id.clone());
            }
        } else {
            ensure!(
                restart.is_none(),
                "再次发送必须选择原文件和完全相同的条款；已有新的一次请先查询"
            );
        }
        let intent_id = uuid::Uuid::new_v4().to_string();
        entries.insert(
            fingerprint,
            TransferIntent {
                intent_id: intent_id.clone(),
                network_id: network,
                client_id: client,
                target_node: target,
                terms,
            },
        );
        vibeface_bridge::platform::privatefs::write_atomic(&path, &serde_json::to_vec(&entries)?)?;
        Ok(intent_id)
    })
    .await?
}
fn validate_order_budget(order: &TransferOrder, budget: &TransferBudget) -> Result<()> {
    validate_transfer_budget(budget)?;
    ensure!(
        budget.state == "open"
            && order.payment.budget_id.as_deref() == Some(budget.budget_id.as_str())
            && budget.provider_peer == order.provider_peer
            && order
                .payment
                .payer
                .as_deref()
                .is_some_and(|payer| payer.eq_ignore_ascii_case(&budget.payer))
            && order
                .payment
                .provider_wallet
                .as_deref()
                .is_some_and(|payee| payee.eq_ignore_ascii_case(&budget.payee))
            && order
                .payment
                .asset
                .as_deref()
                .is_some_and(|asset| asset.eq_ignore_ascii_case(&budget.asset))
            && order.payment.gas_cap == budget.gas_cap
            && transfer_amount_at_most(&order.payment.total, &budget.free),
        "原订单与当前预算角色、资产、可用金额或链费授权不一致"
    );
    Ok(())
}

impl Backend {
    pub(super) async fn reviewed_delivery_order(
        &self,
        original: &TransferOrder,
        accept: bool,
    ) -> Result<TransferOrder> {
        validate_transfer_order(original)?;
        let target = self.client()?.target_node();
        ensure!(
            if accept {
                original.receiver_peer == target && original.role == "receiver"
            } else {
                original.source_peer == target && original.role == "sender"
            },
            "原订单不属于当前设备的此项操作"
        );
        let current: TransferOrder = serde_json::from_value(
            self.client()?
                .read(ReadRequest::Delivery(DeliveryRead::Get {
                    order_id: original.order_id.clone(),
                }))
                .await?,
        )?;
        validate_reviewed_terms(original, &current)?;
        let now = vibeface_protocol::now_ms() / 1000;
        ensure!(
            if accept {
                transfer_acceptable(&current)
            } else {
                transfer_confirmable(&current, now)
            },
            "原订单状态或报价已过期，先查询原订单"
        );
        if !accept {
            self.validate_quote(&current).await?;
            if current.mode == "paid" {
                self.check_order_budget(&current).await?;
            }
        }
        Ok(current)
    }
    pub(super) async fn reviewed_delivery_pair(
        &self,
        original: &[TransferOrder; 2],
        accept: bool,
    ) -> Result<[TransferOrder; 2]> {
        validate_transfer_pair(original, accept, vibeface_protocol::now_ms() / 1000)?;
        let current = [
            self.reviewed_delivery_order(&original[0], accept).await?,
            self.reviewed_delivery_order(&original[1], accept).await?,
        ];
        validate_transfer_pair(&current, accept, vibeface_protocol::now_ms() / 1000)?;
        if !accept {
            let budget = self.check_order_budget(&current[0]).await?;
            ensure!(
                transfer_amount_at_most(
                    &transfer_sum(current.iter().map(|order| order.payment.total.as_str()))?,
                    &budget.free
                ),
                "两笔原订单合计超过当前预算可用金额"
            );
        }
        Ok(current)
    }
    pub(super) async fn reviewed_flush_batch(&self, review: &TransferFlush) -> Result<String> {
        let original = &review.budget;
        transfer_flush_review(original, &review.orders)?;
        validate_transfer_budget(original)?;
        ensure!(
            original.state != "closed" && original.provider_peer == self.client()?.target_node(),
            "只有原中转设备可提交此预算的就绪批次"
        );
        let batch_id = vibeface_client::delivery_flush_batch_id(&original.batch_binding())?;
        let current = self
            .fresh_delivery_budgets()
            .await?
            .into_iter()
            .find(|budget| budget.budget_id == original.budget_id)
            .context("原预算未读回，未提交批次")?;
        ensure!(
            current.state != "closed"
                && current.gas_cap == original.gas_cap
                && vibeface_client::delivery_flush_batch_id(&current.batch_binding())? == batch_id,
            "预算固定角色、资产、链费或原就绪片段已变化，未提交批次"
        );
        for expected in &review.orders {
            let current: TransferOrder = serde_json::from_value(
                self.client()?
                    .read(ReadRequest::Delivery(DeliveryRead::Get {
                        order_id: expected.order_id.clone(),
                    }))
                    .await?,
            )?;
            validate_transfer_order(&current)?;
            ensure!(
                same_terms(expected, &current),
                "就绪批次的原订单条款已变化，未提交"
            );
        }
        Ok(batch_id)
    }
}

fn validate_reviewed_terms(original: &TransferOrder, current: &TransferOrder) -> Result<()> {
    validate_transfer_order(current)?;
    ensure!(
        same_terms(original, current),
        "订单条款或预算已变化，未确认原操作"
    );
    Ok(())
}

#[cfg(test)]
mod prepaid_tests {
    use super::*;
    fn terms(budget: &str) -> TransferTerms {
        TransferTerms {
            receiver: "b".repeat(64),
            provider: "c".repeat(64),
            mode: "paid".into(),
            acceptance: "recoverable".into(),
            name: "file.bin".into(),
            bytes: 4,
            sha256: "f".repeat(64),
            service_cap: "100".into(),
            gas_cap: "7".into(),
            budget_id: Some(budget.into()),
        }
    }
    fn order() -> TransferOrder {
        serde_json::from_value(json!({"order_id":"d".repeat(64),"intent_id":"00000000-0000-4000-8000-000000000001","role":"sender","mode":"paid","acceptance":"recoverable","source_peer":"a".repeat(64),"receiver_peer":"b".repeat(64),"provider_peer":"c".repeat(64),"name":"file.bin","bytes":4,"sha256":"f".repeat(64),"phase":"prepared","evidence":"none","windows":[{"index":0,"bytes":4,"committed_bytes":0,"state":"pending","decryption_proven":false}],"deadlines":{"quote_expires_at":2000000000,"hold_deadline":2000000100,"key_deadline":2000000200,"open_deadline":2000000300,"retain_until":2000000400,"priority_deadline":2000000150},"payment":{"chain_id":"1","asset":format!("0x{}","3".repeat(40)),"payer":format!("0x{}","5".repeat(40)),"provider_wallet":format!("0x{}","4".repeat(40)),"budget_id":"2".repeat(64),"state":"prepared","delivery_fee":"5","reservation_fee":"2","priority_fee":"3","total":"10","gas_cap":"7","earned":"0","withdrawn":"0","refunded":"0"}})).unwrap()
    }
    fn budget() -> TransferBudget {
        TransferBudget {
            budget_id: "2".repeat(64),
            payer: format!("0x{}", "5".repeat(40)),
            payee: format!("0x{}", "4".repeat(40)),
            provider_peer: "c".repeat(64),
            asset: format!("0x{}", "3".repeat(40)),
            deposit: "100".into(),
            free: "100".into(),
            locked: "0".into(),
            earned: "0".into(),
            withdrawn: "0".into(),
            refunded: "0".into(),
            state: "open".into(),
            gas_cap: "7".into(),
            ready_windows: vec![],
        }
    }
    #[tokio::test]
    async fn changing_budget_never_reuses_or_repairs_another_intent() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("native-prepaid-intents-{}", uuid::Uuid::new_v4()));
        vibeface_bridge::platform::privatefs::create_dir(&root)?;
        let path = root.join("delivery-intents.json");
        let a = terms(&"2".repeat(64));
        let b = terms(&"3".repeat(64));
        let first = remember_transfer_intent(
            path.clone(),
            "n".into(),
            "c".into(),
            "t".into(),
            a.clone(),
            None,
        )
        .await?;
        assert_eq!(
            remember_transfer_intent(
                path.clone(),
                "n".into(),
                "c".into(),
                "t".into(),
                a.clone(),
                None
            )
            .await?,
            first
        );
        let second = remember_transfer_intent(
            path.clone(),
            "n".into(),
            "c".into(),
            "t".into(),
            b.clone(),
            None,
        )
        .await?;
        assert_ne!(first, second);
        let mut entries = read_intents(path.clone()).await?;
        assert_eq!(entries.len(), 2);
        entries
            .values_mut()
            .find(|entry| entry.intent_id == first)
            .unwrap()
            .terms = b;
        vibeface_bridge::platform::privatefs::write_atomic(&path, &serde_json::to_vec(&entries)?)?;
        assert!(
            remember_transfer_intent(path.clone(), "n".into(), "c".into(), "t".into(), a, None)
                .await
                .is_err()
        );
        assert_eq!(read_intents(path).await?.len(), 2);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[tokio::test]
    async fn legacy_charity_intent_keeps_its_original_id_without_a_budget_field() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("native-charity-intent-{}", uuid::Uuid::new_v4()));
        vibeface_bridge::platform::privatefs::create_dir(&root)?;
        let path = root.join("delivery-intents.json");
        let legacy = json!({"receiver":"b".repeat(64),"provider":"c".repeat(64),"mode":"charity","acceptance":"recoverable","name":"file.bin","bytes":4,"sha256":"f".repeat(64),"service_cap":"0","gas_cap":"0"});
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(
                &json!({"network":"n","client":"c","target":"t","terms":legacy})
            )?)
        );
        let original = "00000000-0000-4000-8000-000000000001";
        let mut entries = BTreeMap::new();
        entries.insert(fingerprint,json!({"intent_id":original,"network_id":"n","client_id":"c","target_node":"t","terms":legacy}));
        vibeface_bridge::platform::privatefs::write_atomic(&path, &serde_json::to_vec(&entries)?)?;
        let terms: TransferTerms = serde_json::from_value(legacy)?;
        assert_eq!(
            remember_transfer_intent(
                path.clone(),
                "n".into(),
                "c".into(),
                "t".into(),
                terms,
                None
            )
            .await?,
            original
        );
        assert_eq!(read_intents(path).await?.len(), 1);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn fresh_quote_budget_and_caps_cannot_drift_from_the_original_review() -> Result<()> {
        let original = order();
        let budget = budget();
        validate_transfer_order(&original)?;
        validate_order_budget(&original, &budget)?;
        let mut changed = original.clone();
        changed.payment.budget_id = Some("3".repeat(64));
        assert!(validate_reviewed_terms(&original, &changed).is_err());
        assert!(validate_order_budget(&changed, &budget).is_err());
        let mut changed = original.clone();
        changed.payment.delivery_fee = "6".into();
        changed.payment.total = "11".into();
        assert!(validate_reviewed_terms(&original, &changed).is_err());
        let mut changed = budget.clone();
        changed.free = "9".into();
        changed.locked = "91".into();
        assert!(validate_order_budget(&original, &changed).is_err());
        let mut changed = budget;
        changed.payee = format!("0x{}", "6".repeat(40));
        assert!(validate_order_budget(&original, &changed).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;
    fn terms() -> TransferTerms {
        TransferTerms {
            receiver: "b".repeat(64),
            provider: "c".repeat(64),
            mode: "paid".into(),
            acceptance: "recoverable".into(),
            name: "file.bin".into(),
            bytes: 4,
            sha256: "f".repeat(64),
            service_cap: "100".into(),
            gas_cap: "7".into(),
            budget_id: Some("2".repeat(64)),
        }
    }
    fn terminal(id: &str) -> TransferOrder {
        serde_json::from_value(json!({"order_id":"d".repeat(64),"intent_id":id,"role":"sender",
            "mode":"paid","acceptance":"recoverable","source_peer":"a".repeat(64),
            "receiver_peer":"b".repeat(64),"provider_peer":"c".repeat(64),"name":"file.bin",
            "bytes":4,"sha256":"f".repeat(64),"phase":"paid","evidence":"recoverable",
            "windows":[{"index":0,"bytes":4,"committed_bytes":4,"state":"earned","decryption_proven":false}],
            "deadlines":{"quote_expires_at":2000000000,"hold_deadline":2000000100,"key_deadline":2000000200,
                "open_deadline":2000000300,"retain_until":2000000400,"priority_deadline":2000000150},
            "payment":{"chain_id":"1","asset":format!("0x{}","3".repeat(40)),"payer":format!("0x{}","5".repeat(40)),
                "provider_wallet":format!("0x{}","4".repeat(40)),"budget_id":"2".repeat(64),"state":"earned",
                "delivery_fee":"5","reservation_fee":"2","priority_fee":"3","total":"10","gas_cap":"7",
                "earned":"10","withdrawn":"0","refunded":"0","settlement":null}})).unwrap()
    }
    #[tokio::test]
    async fn ordinary_prepare_reuses_intent_and_only_explicit_terminal_restart_replaces_it()
    -> Result<()> {
        let root = std::env::temp_dir().join(format!("native-restart-{}", uuid::Uuid::new_v4()));
        vibeface_bridge::platform::privatefs::create_dir(&root)?;
        let path = root.join("delivery-intents.json");
        let target = "a".repeat(64);
        let first = remember_transfer_intent(
            path.clone(),
            "network".into(),
            "client".into(),
            target.clone(),
            terms(),
            None,
        )
        .await?;
        assert_eq!(
            remember_transfer_intent(
                path.clone(),
                "network".into(),
                "client".into(),
                target.clone(),
                terms(),
                None
            )
            .await?,
            first
        );
        let order = terminal(&first);
        validate_transfer_order(&order)?;
        let next = remember_transfer_intent(
            path.clone(),
            "network".into(),
            "client".into(),
            target.clone(),
            terms(),
            Some(order.clone()),
        )
        .await?;
        assert_ne!(next, first);
        assert_eq!(read_intents(path.clone()).await?.len(), 1);
        assert_eq!(
            remember_transfer_intent(
                path.clone(),
                "network".into(),
                "client".into(),
                target.clone(),
                terms(),
                None
            )
            .await?,
            next
        );
        let unchanged = std::fs::read(&path)?;
        assert!(
            remember_transfer_intent(
                path.clone(),
                "network".into(),
                "client".into(),
                target,
                terms(),
                Some(order)
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read(&path)?,
            unchanged,
            "repeated old restart cannot create a third operation"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[tokio::test]
    async fn unknown_or_changed_file_caps_budget_and_profile_cannot_mint_restart_intents()
    -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("native-restart-denied-{}", uuid::Uuid::new_v4()));
        vibeface_bridge::platform::privatefs::create_dir(&root)?;
        let path = root.join("delivery-intents.json");
        let target = "a".repeat(64);
        let first = remember_transfer_intent(
            path.clone(),
            "network".into(),
            "client".into(),
            target.clone(),
            terms(),
            None,
        )
        .await?;
        let original = terminal(&first);
        let unchanged = std::fs::read(&path)?;
        let mut unknown = original.clone();
        unknown.phase = "reconciling".into();
        unknown.payment.state = "unknown".into();
        assert!(
            remember_transfer_intent(
                path.clone(),
                "network".into(),
                "client".into(),
                target.clone(),
                terms(),
                Some(unknown)
            )
            .await
            .is_err()
        );
        for changed in [
            "name",
            "sha256",
            "bytes",
            "service_cap",
            "gas_cap",
            "budget",
        ] {
            let mut altered = terms();
            match changed {
                "name" => altered.name = "different.bin".into(),
                "sha256" => altered.sha256 = "e".repeat(64),
                "bytes" => altered.bytes = 5,
                "service_cap" => altered.service_cap = "101".into(),
                "gas_cap" => altered.gas_cap = "8".into(),
                _ => altered.budget_id = Some("3".repeat(64)),
            }
            assert!(
                remember_transfer_intent(
                    path.clone(),
                    "network".into(),
                    "client".into(),
                    target.clone(),
                    altered,
                    Some(original.clone())
                )
                .await
                .is_err()
            );
        }
        for (network, client, node) in [
            ("other-network", "client", target.clone()),
            ("network", "other-client", target.clone()),
            ("network", "client", "e".repeat(64)),
        ] {
            assert!(
                remember_transfer_intent(
                    path.clone(),
                    network.into(),
                    client.into(),
                    node,
                    terms(),
                    Some(original.clone())
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            std::fs::read(&path)?,
            unchanged,
            "all denials retain the original durable mapping"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn paid_restart_waits_for_the_whole_order_bonus_to_settle() {
        let mut pending = terminal("00000000-0000-4000-8000-000000000001");
        pending.payment.earned = "7".into();
        assert_eq!(pending.payment.priority_fee, "3");
        assert_eq!(pending.payment.total, "10");
        assert!(validate_transfer_order(&pending).is_ok());
        assert!(
            !can_restart_transfer(&pending),
            "earned windows do not settle the pending whole-order bonus"
        );

        let mut earned = pending.clone();
        earned.payment.earned = "10".into();
        assert!(can_restart_transfer(&earned));

        let mut refunded = pending;
        refunded.payment.refunded = "3".into();
        assert!(can_restart_transfer(&refunded));
    }

    #[test]
    fn restart_requires_exact_terminal_evidence_and_recoverable_acceptance() {
        let order = terminal("00000000-0000-4000-8000-000000000001");
        assert!(can_restart_transfer(&order));
        let mut refunded = order.clone();
        refunded.phase = "refunded".into();
        refunded.payment.state = "refunded".into();
        refunded.payment.earned = "0".into();
        refunded.payment.refunded = "10".into();
        assert!(can_restart_transfer(&refunded));
        refunded.payment.refunded = "9".into();
        assert!(!can_restart_transfer(&refunded));
        refunded.payment.refunded = "10".into();
        refunded.payment.state = "unknown".into();
        assert!(!can_restart_transfer(&refunded));
        let mut changed = order.clone();
        changed.windows[0].state = "pending".into();
        assert!(!can_restart_transfer(&changed));
        let mut changed = order.clone();
        changed.role = "receiver".into();
        assert!(!can_restart_transfer(&changed));
        let mut changed = order;
        changed.acceptance = "strict".into();
        assert!(!can_restart_transfer(&changed));
    }
}
