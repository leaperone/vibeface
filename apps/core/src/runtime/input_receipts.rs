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
            self.store
                .call("input_receipts.resolve_input_target", move |store| {
                    store.resolve_input_target(&network, &target)
                })
                .await
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
        let local = self.identity.device_id()?;
        if let Some(original) = input.get("original_input") {
            ensure!(
                input["entry_device_id"] == local,
                "input reconciliation requires its original entry Node"
            );
            ensure!(
                original["message_id"] == message_id
                    && original["conversation_id"] == conversation
                    && original["target"] == serde_json::to_value(&target)?,
                "original input identity does not match the receipt query"
            );
            let (network, client, local, original) = (
                network.to_owned(),
                field(input, "client_id")?.to_owned(),
                local.clone(),
                original.clone(),
            );
            if let Some(value) = self
                .store
                .call("input_receipts.reconcile_original", move |store| {
                    store.client_input_receipt(&network, &client, &local, &original)
                })
                .await?
            {
                if value["code"] != "input_not_saved" {
                    receipt_identity(&value, &message_id, &target)?;
                }
                return Ok(value);
            }
        }
        let resolved = {
            let (network, message_id, conversation, target) = (
                network.to_owned(),
                message_id.clone(),
                conversation.clone(),
                target.clone(),
            );
            self.store
                .call("input_receipts.resolve", move |store| {
                    let original = store
                        .workspace_input(&network, &message_id)?
                        .context("input not found")?;
                    ensure!(
                        original.conversation_id == conversation
                            && original.input.route.target == target,
                        "input target does not match the saved message"
                    );
                    store.resolve_input_target(&network, &target)
                })
                .await?
        };
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
    use crate::runtime::test_support::{access, join_network, open_node, signed_main_input};

    #[tokio::test]
    async fn original_entry_reconciliation_requires_exact_authority_and_retained_absence()
    -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("input-receipt-{}", uuid::Uuid::new_v4()));
        let configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        let node = open_node(&directory, &configuration).await?;
        let result = async {
            let (admin, network) = join_network(&node).await?;
            super::super::triage::tests::bot(&node, &admin, "chat", "Receipt fixture", &[]).await?;
            let local = node.identity.device_id()?;
            let client = admin.device_id()?;
            let access = access(network.clone(), true, Some("chat".into()));
            let signed = signed_main_input(&admin, &network, "chat", "missing", "original input", vec![], 1)?;
            let original = json!({"conversation_id":"chat","message_id":"missing",
                "target":{"kind":"bot_main","bot_id":"chat"},"intent":"continue","sequence":1,
                "text":"original input","report_results":true,"attestation":signed.attestation()});
            let query = json!({"conversation_id":"chat","message_id":"missing","target":original["target"],
                "entry_device_id":local,"client_id":client,"original_input":original});
            let not_saved = node.input_receipt(&access, &query).await?;
            assert_eq!(not_saved["code"], "input_not_saved");
            assert_eq!(not_saved["stored"], false);
            assert_eq!(not_saved["entry_device_id"], local);
            assert_eq!(not_saved["network_id"], network);
            assert_eq!(not_saved["client_id"], client);
            assert_eq!(not_saved["conversation_id"], "chat");
            assert_eq!(not_saved["message_id"], "missing");
            assert_eq!(not_saved["target"], original["target"]);
            let other_network = crate::runtime::test_support::access("0".repeat(64), true, Some("chat".into()));
            assert!(node.input_receipt(&other_network, &query).await.is_err());
            for (pointer, value) in [
                ("/entry_device_id", json!("another-entry")),
                ("/client_id", json!(local)),
                ("/target/bot_id", json!("another-bot")),
                ("/original_input/text", json!("changed signed input")),
                ("/original_input/report_results", Value::Null),
            ] {
                let mut invalid = query.clone();
                *invalid.pointer_mut(pointer).context("fixture pointer")? = value;
                assert!(node.input_receipt(&access, &invalid).await.is_err(), "{pointer}");
            }
            let mut unbound = query.clone();
            unbound.as_object_mut().context("fixture query")?.remove("original_input");
            assert!(node.input_receipt(&access, &unbound).await.is_err(), "plain absence grants no replay authority");

            // The original may arrive after the absence reply and race the person's exact retry.
            let (late, retry) = tokio::join!(node.accept_chat_message(&access, &original), node.accept_chat_message(&access, &original));
            assert_eq!(late?["accepted"], true);
            assert_eq!(retry?["accepted"], true);
            let receipt = node.input_receipt(&access, &query).await?;
            assert_ne!(receipt["stage"], "unknown");
            assert!(receipt.get("code").is_none(), "existing positive receipts are preserved");
            node.store.call("fixture_input_deduplication", {
                let network = network.clone();
                move |store| {
                    assert_eq!(store.connection.query_row("SELECT count(*) FROM events WHERE network_id=? AND kind='input_posted' AND subject='missing'", [&network], |row| row.get::<_, u64>(0))?, 1);
                    assert_eq!(store.scheduler_jobs("chat")?.len(), 1);
                    Ok(())
                }
            }).await?;

            for case in ["ineffective", "pending", "peer"] {
                let body = signed_main_input(&admin, &network, "chat", case, "retained input", vec![], 2)?;
                let original = json!({"conversation_id":"chat","message_id":case,
                    "target":{"kind":"bot_main","bot_id":"chat"},"intent":"continue","sequence":2,
                    "text":"retained input","report_results":true,"attestation":body.attestation()});
                node.store.call("fixture_retained_input_evidence", {
                    let network = network.clone();
                    let identity = node.identity.clone();
                    move |store| {
                        match case {
                            "ineffective" => {
                                let event = store.append_event(&identity, &network, "chat", body, vec![], 10)?;
                                store.connection.execute("UPDATE events SET effective=0 WHERE network_id=? AND hash=?", rusqlite::params![network, event.hash])?;
                                store.connection.execute("DELETE FROM messages WHERE network_id=? AND message_id=?", rusqlite::params![network, case])?;
                            }
                            "pending" => {
                                let event = identity.sign_event(crate::model::Event {
                                    version: crate::model::WIRE_VERSION, network_id: network.clone(), author: identity.device_id()?,
                                    seq: 1, prev: None, deps: vec!["f".repeat(64)], scope_id: "chat".into(),
                                    created_at_ms: 11, body, blobs: vec![],
                                })?;
                                assert_eq!(store.ingest_events(&network, &[event])?.statuses, [crate::model::IngestStatus::Pending]);
                            }
                            "peer" => {
                                use crate::store::admission::{PeerCommand, PeerEnvelope};
                                let payload = PeerCommand::Deliver { conversation_id: "chat".into(), trigger: crate::scheduling::message_trigger(case) };
                                store.save_peer_command(&network, &identity.device_id()?, &PeerEnvelope {
                                    command_id: payload.id(&network), target: "main".into(), owner_epoch: 1, revision: None,
                                    sequence: 2, causal_refs: vec!["e".repeat(64)], payload,
                                }, 12)?;
                            }
                            _ => unreachable!(),
                        }
                        assert!(store.workspace_input(&network, case)?.is_none());
                        Ok(())
                    }
                }).await?;
                let query = json!({"conversation_id":"chat","message_id":case,"target":original["target"],
                    "entry_device_id":local,"client_id":client,"original_input":original});
                let receipt = node.input_receipt(&access, &query).await?;
                assert_eq!(receipt["stage"], "unknown", "{case} cannot authorize replay");
                assert_eq!(receipt["retry_safe"], false);
                let rejected = node.accept_chat_message(&access, &original).await?;
                assert_eq!(rejected["code"], "input_conflict", "{case} cannot be revived by a late retry");
                assert_eq!(rejected["stored"], true);
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(directory)?;
        result
    }

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
