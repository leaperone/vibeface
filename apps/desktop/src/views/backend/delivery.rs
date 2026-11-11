use super::*;
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
    pub(super) async fn delivery_intent(&self, terms: Value) -> Result<String> {
        let client = self.client()?;
        let terms: TransferTerms = serde_json::from_value(terms)?;
        let target = self
            .active_entry
            .as_ref()
            .context("当前设备未确认")?
            .node_id
            .clone();
        let path = self.client_directory()?.join("delivery-intents.json");
        remember_transfer_intent(
            path,
            client.network_id().into(),
            client.client_id().into(),
            target,
            terms,
        )
        .await
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
async fn read_intents(path: PathBuf) -> Result<BTreeMap<String, TransferIntent>> {
    tokio::task::spawn_blocking(move || {
        if !path.try_exists()? {
            return Ok(BTreeMap::new());
        }
        let bytes = vibeface_bridge::platform::privatefs::read_private_bounded(
            &path,
            4 * 1024 * 1024,
            "文件传输意图",
        )?;
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await?
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
) -> Result<String> {
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"network":network,"client":client,"target":target,"terms":terms})
        )?)
    );
    let mut entries = read_intents(path.clone()).await?;
    if let Some(intent) = entries.get(&fingerprint) {
        ensure!(
            intent.network_id == network
                && intent.client_id == client
                && intent.target_node == target
                && intent.terms == terms,
            "本机发送记录与原预算或条款不一致，未创建第二笔意图"
        );
        return Ok(intent.intent_id.clone());
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
    tokio::task::spawn_blocking(move || {
        vibeface_bridge::platform::privatefs::write_atomic(&path, &serde_json::to_vec(&entries)?)
    })
    .await??;
    Ok(intent_id)
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
        let first =
            remember_transfer_intent(path.clone(), "n".into(), "c".into(), "t".into(), a.clone())
                .await?;
        assert_eq!(
            remember_transfer_intent(path.clone(), "n".into(), "c".into(), "t".into(), a.clone())
                .await?,
            first
        );
        let second =
            remember_transfer_intent(path.clone(), "n".into(), "c".into(), "t".into(), b.clone())
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
            remember_transfer_intent(path.clone(), "n".into(), "c".into(), "t".into(), a)
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
            remember_transfer_intent(path.clone(), "n".into(), "c".into(), "t".into(), terms)
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
