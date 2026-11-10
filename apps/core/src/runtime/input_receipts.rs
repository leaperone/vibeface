use super::*;
use crate::workspace::InputTarget;

fn receipt_identity(value: &Value, message_id: &str, target: &InputTarget) -> Result<()> {
    ensure!(
        value["message_id"] == message_id,
        "input receipt has another message ID"
    );
    ensure!(
        value["target"] == serde_json::to_value(target)?,
        "input receipt has another target"
    );
    ensure!(
        matches!(
            value["stage"].as_str(),
            Some(
                "entry_accepted"
                    | "owner_accepted"
                    | "submitting"
                    | "provider_accepted"
                    | "associated"
                    | "refused"
                    | "unknown"
            )
        ),
        "invalid input receipt stage"
    );
    ensure!(
        value["retry_safe"].is_boolean(),
        "input receipt has no retry evidence"
    );
    let control = value
        .get("control_event")
        .context("input receipt has no control reference")?;
    if let Some(hash) = control.as_str() {
        crate::crypto::decode32(hash)?;
    } else {
        ensure!(control.is_null(), "invalid input control reference");
    }
    ensure!(
        value["retry_safe"] != true || control.is_string(),
        "safe retry has no exact input control reference"
    );
    ensure!(
        value["stage"] != "unknown" || value["retry_safe"] == false,
        "unknown input receipt cannot authorize a retry"
    );
    Ok(())
}

impl Runtime {
    pub(super) async fn scope_context(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "scope context access requires the user");
        let network = &access.network_id;
        let conversation = id(input, "conversation_id")?;
        self.require_scope(access, &conversation).await?;
        let target: InputTarget =
            serde_json::from_value(input["target"].clone()).context("invalid input target")?;
        let resolved = {
            let network = network.to_owned();
            let target = target.to_owned();
            self.store.call("input_receipts.resolve_input_target", move |store| store.resolve_input_target(&network, &target)).await
        }?;
        ensure!(
            resolved.conversation_id == conversation,
            "context target belongs to another conversation"
        );
        let local = self.identity.device_id()?;
        let value = if resolved.owner_node == local {
            self.read_scope_context(network, input).await?
        } else {
            match self
                .peer_request(
                    network,
                    &resolved.owner_node,
                    json!({"type":"scope_context", "conversation_id":conversation,
                        "target":target}),
                )
                .await
            {
                Ok(value) => value,
                Err(_) => json!({"conversation_id":conversation,"target":target,
                    "available":false,"context":null,"reason":"执行设备的本轮上下文暂时无法查询。"}),
            }
        };
        ensure!(
            value["conversation_id"] == conversation
                && value["target"] == serde_json::to_value(&target)?,
            "scope context has another target"
        );
        ensure!(
            value["available"].is_boolean(),
            "invalid scope context availability"
        );
        ensure!(
            value["available"] != true || value["context"].is_object(),
            "available scope context has no evidence"
        );
        Ok(value)
    }

    pub(super) async fn input_receipt(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "input receipt access requires the user");
        let network = &access.network_id;
        let message_id = id(input, "message_id")?;
        let conversation = id(input, "conversation_id")?;
        self.require_scope(access, &conversation).await?;
        let target: InputTarget =
            serde_json::from_value(input["target"].clone()).context("invalid input target")?;
        let resolved = {
            let (network, message_id, conversation, target) = (network.to_owned(), message_id.clone(), conversation.clone(), target.clone());
            self.store.call("input_receipts.resolve", move |store| {
                let original = store
                    .workspace_input(&network, &message_id)?
                    .context("input not found")?;
                ensure!(
                    original.conversation_id == conversation && original.input.route.target == target,
                    "input target does not match the saved message"
                );
                store.resolve_input_target(&network, &target)
            }).await?
        };
        let local = self.identity.device_id()?;
        let value = if resolved.owner_node == local {
            self.read_input_receipt(network, input).await?
        } else {
            match self
                .peer_request(
                    network,
                    &resolved.owner_node,
                    json!({"type":"input_receipt", "conversation_id":conversation,
                        "message_id":message_id, "target":target}),
                )
                .await
            {
                Ok(value) => value,
                Err(_) => json!({"message_id":message_id,"target":target,"stage":"unknown",
                    "actual_turn_id":null,"control_event":null,"reason":"执行设备接收状态尚未确认。","retry_safe":false}),
            }
        };
        receipt_identity(&value, &message_id, &target)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_cannot_change_the_message_target_or_retry_an_unknown_input() -> Result<()> {
        let target = InputTarget::Task {
            task_id: "task-a".into(),
        };
        let mut value = json!({"message_id":"m1","target":target,"stage":"unknown",
            "actual_turn_id":null,"control_event":null,"reason":null,"retry_safe":false});
        receipt_identity(&value, "m1", &target)?;
        value["target"]["task_id"] = json!("task-b");
        assert!(receipt_identity(&value, "m1", &target).is_err());
        value["target"] = serde_json::to_value(&target)?;
        value["retry_safe"] = json!(true);
        assert!(receipt_identity(&value, "m1", &target).is_err());
        value["stage"] = json!("refused");
        assert!(receipt_identity(&value, "m1", &target).is_err());
        value["control_event"] = json!("a".repeat(64));
        receipt_identity(&value, "m1", &target)?;
        value["retry_safe"] = json!(false);
        assert!(receipt_identity(&value, "m2", &target).is_err());
        Ok(())
    }
}
