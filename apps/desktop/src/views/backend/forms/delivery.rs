use super::*;

impl Backend {
    pub(super) async fn save_delivery(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-delivery-enter");
        let get = |key: &str| values.get(key).cloned().unwrap_or_default();
        {
            ensure!(self.state.can_manage, "仅管理设备可准备订单");
            let mode: vibeface_client::DeliveryMode = serde_json::from_value(json!(get("mode")))?;
            ensure!(
                get("acceptance").is_empty() || get("acceptance") == "recoverable",
                "新传输仅支持可恢复验收，不再准备Strict订单"
            );
            let acceptance = vibeface_client::DeliveryAcceptance::Recoverable;
            let budget_id = if matches!(mode, vibeface_client::DeliveryMode::Paid) {
                let id = get("budget_id");
                ensure!(!id.is_empty(), "请从已有开放预算选择受保护付费传输");
                self.prepare_budget(
                    &id,
                    &get("provider_peer"),
                    &get("max_service_amount"),
                    &get("max_gas_amount"),
                )
                .await?;
                Some(id)
            } else {
                None
            };
            let path = PathBuf::from(get("file"));
            let metadata = tokio::fs::metadata(&path).await?;
            ensure!(
                self.state.delivery.enabled && metadata.len() <= self.state.delivery.max_file_bytes,
                "传输未开启或文件超过允许大小"
            );
            let name = path
                .file_name()
                .context("文件名无效")?
                .to_string_lossy()
                .into_owned();
            let content = tokio::fs::read(&path).await?;
            ensure!(
                content.len() as u64 == metadata.len(),
                "文件读取过程中大小改变"
            );
            let restart_order = get("_restart_order_id");
            let restart_intent = get("_restart_intent_id");
            ensure!(
                restart_order.is_empty() == restart_intent.is_empty(),
                "再次发送的原始上下文不完整"
            );
            let restart_review = get("_restart_review");
            ensure!(
                restart_order.is_empty() == restart_review.is_empty(),
                "再次发送的原订单核对内容不完整"
            );
            let restart: Option<TransferOrder> = if restart_order.is_empty() {
                None
            } else {
                let review: TransferOrder = serde_json::from_str(&restart_review)?;
                ensure!(
                    review.order_id == restart_order && review.intent_id == restart_intent,
                    "再次发送的原订单与固定上下文不同"
                );
                Some(review)
            };
            let intent_id=self.delivery_intent(json!({"receiver":get("receiver_peer"),"provider":get("provider_peer"),"mode":get("mode"),"acceptance":"recoverable","name":name,"bytes":content.len(),"sha256":format!("{:x}",Sha256::digest(&content)),"service_cap":get("max_service_amount"),"gas_cap":get("max_gas_amount"),"budget_id":budget_id}),restart).await?;
            self.submit(Action::Delivery(vibeface_client::DeliveryAction::Prepare(
                vibeface_client::DeliveryPrepare {
                    intent_id,
                    receiver_peer: get("receiver_peer"),
                    provider_peer: get("provider_peer"),
                    mode,
                    acceptance,
                    name,
                    content_base64: base64::engine::general_purpose::STANDARD.encode(content),
                    max_service_amount: get("max_service_amount"),
                    max_gas_amount: get("max_gas_amount"),
                    budget_id,
                },
            )))
            .await?;
        }
        Ok(())
    }
}
