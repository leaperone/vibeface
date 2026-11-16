//! Exact signed request receipts; only original-entry input reconciliation can prove absence.
use super::*;
use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand};
use serde_json::json;

impl Store {
    pub(crate) fn client_input_receipt(
        &self,
        network: &str,
        client_id: &str,
        entry_device_id: &str,
        original: &Value,
    ) -> Result<Option<Value>> {
        let conversation = original["conversation_id"]
            .as_str()
            .context("original conversation is missing")?;
        let input: crate::workspace::InputRequest = serde_json::from_value(original.clone())?;
        let attestation: Attestation = serde_json::from_value(original["attestation"].clone())?;
        vibeface_protocol::validate_id(&input.message_id).map_err(anyhow::Error::msg)?;
        ensure!(input.route.sequence > 0, "invalid input sequence");
        ensure!(
            attestation.device_id == client_id,
            "receipt belongs to another client"
        );
        membership_from(&self.connection, network)?.client(client_id)?;
        let body = crate::workspace::input_event_body(&input, Some(attestation.clone()));
        crate::crypto::verify_attestation(network, conversation, &body)?;
        let resolved = self.resolve_input_target(network, &input.route.target)?;
        ensure!(
            resolved.conversation_id == conversation,
            "input target belongs to another conversation"
        );
        if let Some(saved) = self.workspace_input(network, &input.message_id)? {
            ensure!(
                saved.conversation_id == conversation && saved.input == input,
                "input ID already has another signed intent"
            );
            let source = log::event_from(&self.connection, network, &saved.source_event)?
                .context("input event is missing")?;
            ensure!(
                source.body.attestation() == Some(&attestation),
                "input ID belongs to another signer"
            );
            return Ok(None);
        }
        if self.input_evidence(network, conversation, &input.message_id)? {
            return Ok(Some(
                json!({"message_id":input.message_id,"target":input.route.target,
                "stage":"unknown","actual_turn_id":null,"control_event":null,
                "reason":"这条输入仍有记录，暂时无法确认是否保存。","retry_safe":false}),
            ));
        }
        Ok(Some(
            json!({"accepted":false,"code":"input_not_saved","stored":false,
            "entry_device_id":entry_device_id,"network_id":network,"client_id":client_id,
            "conversation_id":conversation,"message_id":input.message_id,"target":input.route.target,
            "reason":"原设备未保存这条输入，可选择重试原输入或不再发送。"}),
        ))
    }

    pub fn client_workspace_receipt(
        &self,
        network: &str,
        client_id: &str,
        command_id: &str,
        command: &WorkspaceCommand,
        attestation: &Attestation,
    ) -> Result<Value> {
        vibeface_protocol::validate_id(command_id).map_err(anyhow::Error::msg)?;
        ensure!(
            attestation.device_id == client_id,
            "receipt belongs to another client"
        );
        let body = EventBody::WorkspaceCommanded {
            command_id: command_id.into(),
            command: command.clone(),
            acting_assignment: None,
            attestation: Some(attestation.clone()),
            cause: None,
        };
        crate::crypto::verify_attestation(network, WORKSPACE_SCOPE, &body)?;
        let expected = crate::crypto::attestation_payload(network, WORKSPACE_SCOPE, &body)?;
        let mut query = self.connection.prepare(
            "SELECT hash,envelope FROM events WHERE network_id=?1 AND scope_id=?2 AND kind='workspace_commanded' AND subject=?3 AND effective=1 ORDER BY lamport,created_at_ms,hash LIMIT 256"
        )?;
        let rows = query.query_map(params![network, WORKSPACE_SCOPE, command_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (hash, envelope) = row?;
            let signed: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&signed.canonical_body)?;
            if event.network_id == network
                && event.scope_id == WORKSPACE_SCOPE
                && event.body.attestation() == Some(attestation)
                && crate::crypto::attestation_payload(network, WORKSPACE_SCOPE, &event.body)?
                    == expected
            {
                return Ok(
                    json!({"network_id":network,"client_id":client_id,"command_id":command_id,"receipt":"recorded","event_hash":hash}),
                );
            }
        }
        Ok(
            json!({"network_id":network,"client_id":client_id,"command_id":command_id,"receipt":"not_seen_here"}),
        )
    }
}

impl Store {
    pub fn client_signed_receipt(
        &self,
        network: &str,
        client_id: &str,
        scope_id: &str,
        mut body: Value,
        attestation: &Attestation,
    ) -> Result<Value> {
        vibeface_protocol::validate_id(scope_id).map_err(anyhow::Error::msg)?;
        ensure!(
            attestation.device_id == client_id,
            "receipt belongs to another client"
        );
        let object = body
            .as_object_mut()
            .context("signed body must be an object")?;
        ensure!(
            !object.contains_key("attestation")
                && !object.contains_key("cause")
                && !object.contains_key("acting_assignment"),
            "receipt needs the original human body without execution authority"
        );
        object.insert("attestation".into(), serde_json::to_value(attestation)?);
        let body: EventBody = serde_json::from_value(body)?;
        crate::crypto::verify_attestation(network, scope_id, &body)?;
        let expected = crate::crypto::attestation_payload(network, scope_id, &body)?;
        let subject = body.subject();
        let mut query=self.connection.prepare(
            "SELECT hash,envelope FROM events WHERE network_id=?1 AND scope_id=?2 AND kind=?3 AND subject IS ?4 AND effective=1 ORDER BY lamport DESC,created_at_ms DESC,hash DESC LIMIT 256"
        )?;
        let rows = query.query_map(params![network, scope_id, body.kind(), subject], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (hash, envelope) = row?;
            let signed: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&signed.canonical_body)?;
            if event.network_id == network
                && event.scope_id == scope_id
                && event.body.attestation() == Some(attestation)
                && crate::crypto::attestation_payload(network, scope_id, &event.body)? == expected
            {
                return Ok(
                    json!({"network_id":network,"client_id":client_id,"scope_id":scope_id,"receipt":"recorded","event_hash":hash}),
                );
            }
        }
        Ok(
            json!({"network_id":network,"client_id":client_id,"scope_id":scope_id,"receipt":"not_seen_here"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(body: EventBody, scope: &str) -> Result<(Store, String, String, EventBody)> {
        let node = LocalIdentity::generate();
        let state = node.sign_network_fixture(1, vec![node.device_id()?])?;
        let network = state.genesis.network_id.clone();
        let client = node.fixture_admin();
        let signed = client.attest(&network, scope, body)?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&network, &state)?;
        if scope == WORKSPACE_SCOPE {
            let bot = client.attest(
                &network,
                "chat",
                EventBody::BotCreated {
                    bot_id: "bot".into(),
                    name: "Fixture".into(),
                    instructions: String::new(),
                    project: None,
                    attestation: None,
                    cause: None,
                },
            )?;
            store.create_bot(
                &node,
                &network,
                &BotCreateSpec {
                    bot_id: "bot".into(),
                    conversation_id: "chat".into(),
                    name: "Fixture".into(),
                    instructions: String::new(),
                    project: None,
                    attestation: bot.attestation().context("fixture bot signature")?.clone(),
                    owner: node.owner_fixture(&network, "chat", &node.device_id()?)?,
                },
                1,
            )?;
        }
        store.append_event(&node, &network, scope, signed.clone(), vec![], 2)?;
        Ok((store, network, client.device_id()?, signed))
    }
    #[test]
    fn input_receipt_and_post_preserve_the_original_signed_intent() -> Result<()> {
        use crate::{identity::GovernanceEvent, workspace::InputRequest};

        let node = LocalIdentity::generate();
        let other_entry = LocalIdentity::generate();
        let admin = node.fixture_admin();
        let other_client = LocalIdentity::generate();
        let mut state =
            node.sign_network_fixture(1, vec![node.device_id()?, other_entry.device_id()?])?;
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::client(&other_client, "second-client", false)?,
        )?)?;
        let network = state.genesis.network_id.clone();
        let client_id = admin.device_id()?;
        let entry_id = node.device_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&network, &state)?;
        for (bot_id, conversation) in [("bot", "chat"), ("other-bot", "other-chat")] {
            let bot = admin.attest(
                &network,
                conversation,
                EventBody::BotCreated {
                    bot_id: bot_id.into(),
                    name: bot_id.into(),
                    instructions: String::new(),
                    project: None,
                    attestation: None,
                    cause: None,
                },
            )?;
            store.create_bot(
                &node,
                &network,
                &BotCreateSpec {
                    bot_id: bot_id.into(),
                    conversation_id: conversation.into(),
                    name: bot_id.into(),
                    instructions: String::new(),
                    project: None,
                    attestation: bot.attestation().context("bot signature")?.clone(),
                    owner: node.owner_fixture(&network, conversation, &entry_id)?,
                },
                1,
            )?;
        }
        let input: InputRequest = serde_json::from_value(json!({
            "message_id": "m1", "target": {"kind": "bot_main", "bot_id": "bot"},
            "intent": "defer", "sequence": 17, "text": "hi\n雪e\u{301} ",
            "attachments": [
                {"hash": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae", "size": 3, "name": "一.txt"},
                {"hash": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", "size": 5, "name": "two.txt"}
            ],
            "report_results": false
        }))?;
        let independent: EventBody = serde_json::from_str(
            r#"{"type":"input_posted","message_id":"m1","target":{"kind":"bot_main","bot_id":"bot"},"intent":"defer","sequence":17,"text":"hi\n雪é ","attachments":[{"hash":"2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae","size":3,"name":"一.txt"},{"hash":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","size":5,"name":"two.txt"}],"report_results":false}"#,
        )?;
        let signed = admin.attest(&network, "chat", independent.clone())?;
        let signature = signed.attestation().context("input signature")?.clone();
        let expected_payload = crate::crypto::attestation_payload(&network, "chat", &signed)?;
        let mut original = serde_json::to_value(&input)?;
        original["conversation_id"] = json!("chat");
        original["attestation"] = serde_json::to_value(&signature)?;
        let before_changes = store.connection.total_changes();
        let before_vector = store.version_vector(&network)?;
        let absent = store.client_input_receipt(&network, &client_id, &entry_id, &original)?;
        assert_eq!(
            absent,
            Some(json!({
                "accepted": false, "code": "input_not_saved", "stored": false,
                "entry_device_id": entry_id, "network_id": network, "client_id": client_id,
                "conversation_id": "chat", "message_id": "m1", "target": {"kind": "bot_main", "bot_id": "bot"},
                "reason": "原设备未保存这条输入，可选择重试原输入或不再发送。"
            }))
        );
        assert_eq!(store.connection.total_changes(), before_changes);
        assert_eq!(store.version_vector(&network)?, before_vector);

        store.put_blob(&input.attachments[0].blob(), b"foo")?;
        store.put_blob(&input.attachments[1].blob(), b"hello")?;

        let saved =
            store.post_workspace_input(&node, &network, input.clone(), signature.clone(), 10)?;
        assert_eq!(saved.input, input);
        assert_eq!(saved.conversation_id, "chat");
        let envelope = store
            .event(&network, &saved.source_event)?
            .context("saved event")?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        assert_eq!(envelope.hash, saved.source_event);
        assert_eq!(
            serde_json::to_value(&event.body)?,
            serde_json::to_value(&signed)?
        );
        assert_eq!(
            event.blobs,
            vec![
                BlobRef {
                    hash: "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".into(),
                    size: 3
                },
                BlobRef {
                    hash: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
                    size: 5
                },
            ]
        );
        assert_eq!(
            crate::crypto::attestation_payload(&network, "chat", &event.body)?,
            expected_payload
        );
        crate::crypto::verify_attestation(&network, "chat", &event.body)?;
        assert_eq!(
            store.pending_workspace_input_admissions(&network)?,
            vec!["m1"]
        );
        let saved_changes = store.connection.total_changes();
        let saved_vector = store.version_vector(&network)?;
        assert!(
            store
                .client_input_receipt(&network, &client_id, &entry_id, &original)?
                .is_none()
        );
        let retry = store.post_workspace_input(
            &other_entry,
            &network,
            input.clone(),
            signature.clone(),
            11,
        )?;
        assert_eq!(retry, saved);
        assert_eq!(store.connection.total_changes(), saved_changes);
        assert_eq!(store.version_vector(&network)?, saved_vector);
        assert_eq!(
            store.pending_workspace_input_admissions(&network)?,
            vec!["m1"]
        );
        let input_count: u64 = store.connection.query_row(
            "SELECT COUNT(*) FROM events WHERE network_id=? AND kind='input_posted'",
            [&network],
            |row| row.get(0),
        )?;
        assert_eq!(input_count, 1);

        for (path, value) in [
            ("/message_id", json!("another-id")),
            ("/target/bot_id", json!("other-bot")),
            ("/intent", json!("continue")),
            ("/sequence", json!(18)),
            ("/text", json!("hi\n雪é ")),
            ("/report_results", json!(true)),
            ("/attachments/0/hash", json!("c".repeat(64))),
            ("/attachments/0/size", json!(4)),
            ("/attachments/0/name", json!("renamed.txt")),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(path).context("mutation field")? = value;
            let changed_input: InputRequest = serde_json::from_value(changed.clone())?;
            assert!(
                store
                    .post_workspace_input(&node, &network, changed_input, signature.clone(), 12)
                    .is_err(),
                "{path}"
            );
            assert!(
                store
                    .client_input_receipt(&network, &client_id, &entry_id, &changed)
                    .is_err(),
                "{path}"
            );
            assert_eq!(store.connection.total_changes(), saved_changes, "{path}");
            assert_eq!(store.version_vector(&network)?, saved_vector, "{path}");
        }
        let mut changed = original.clone();
        changed["expected_revision"] = json!(1);
        let changed_input: InputRequest = serde_json::from_value(changed.clone())?;
        assert!(
            store
                .post_workspace_input(&node, &network, changed_input, signature.clone(), 12)
                .is_err()
        );
        assert!(
            store
                .client_input_receipt(&network, &client_id, &entry_id, &changed)
                .is_err()
        );
        let mut reordered = original.clone();
        reordered["attachments"]
            .as_array_mut()
            .context("attachments")?
            .swap(0, 1);
        let reordered_input: InputRequest = serde_json::from_value(reordered.clone())?;
        assert!(
            store
                .post_workspace_input(&node, &network, reordered_input, signature, 12)
                .is_err()
        );
        assert!(
            store
                .client_input_receipt(&network, &client_id, &entry_id, &reordered)
                .is_err()
        );

        let replacement = other_client.attest(&network, "chat", independent)?;
        let replacement_signature = replacement
            .attestation()
            .context("replacement signature")?
            .clone();
        membership_from(&store.connection, &network)?.client(&other_client.device_id()?)?;
        let post_error = store
            .post_workspace_input(&node, &network, input, replacement_signature.clone(), 12)
            .unwrap_err();
        assert_eq!(post_error.to_string(), "input ID belongs to another signer");
        let mut replacement_original = original;
        replacement_original["attestation"] = serde_json::to_value(replacement_signature)?;
        let receipt_error = store
            .client_input_receipt(
                &network,
                &other_client.device_id()?,
                &entry_id,
                &replacement_original,
            )
            .unwrap_err();
        assert_eq!(
            receipt_error.to_string(),
            "input ID belongs to another signer"
        );
        assert_eq!(store.connection.total_changes(), saved_changes);
        assert_eq!(store.version_vector(&network)?, saved_vector);
        assert_eq!(
            store.pending_workspace_input_admissions(&network)?,
            vec!["m1"]
        );
        Ok(())
    }

    #[test]
    fn signed_receipts_verify_exact_client_network_and_payload_without_writing() -> Result<()> {
        let (store, network, client, signed) = fixture(
            EventBody::NotificationAcked {
                notification_id: "notice".into(),
                client_id: "inbox".into(),
                attestation: None,
            },
            "chat",
        )?;
        let attestation = signed.attestation().context("fixture attestation")?.clone();
        let mut body = serde_json::to_value(signed)?;
        body.as_object_mut().context("body")?.remove("attestation");
        let before = store.connection.total_changes();
        let receipt =
            store.client_signed_receipt(&network, &client, "chat", body.clone(), &attestation)?;
        assert_eq!(receipt["receipt"], "recorded");
        assert_eq!(receipt["client_id"], client);
        assert!(
            store
                .client_signed_receipt(
                    &network,
                    &"0".repeat(64),
                    "chat",
                    body.clone(),
                    &attestation
                )
                .is_err()
        );
        assert!(
            store
                .client_signed_receipt(&"0".repeat(64), &client, "chat", body.clone(), &attestation)
                .is_err()
        );
        body["notification_id"] = json!("different");
        assert!(
            store
                .client_signed_receipt(&network, &client, "chat", body, &attestation)
                .is_err()
        );
        assert_eq!(store.connection.total_changes(), before);
        Ok(())
    }
    #[test]
    fn workspace_receipt_matches_the_original_signed_command_and_effective_record() -> Result<()> {
        let command = WorkspaceCommand::CreateTask {
            task_id: "task".into(),
            channel_id: None,
            parent_task_id: None,
            owner_bot: "bot".into(),
            title: "fixture".into(),
            request: "controlled".into(),
            expected_result: "receipt".into(),
            source_events: vec![],
            dependencies: vec![],
            reply_only: false,
        };
        let (store, network, client, signed) = fixture(
            EventBody::WorkspaceCommanded {
                command_id: "command".into(),
                command: command.clone(),
                acting_assignment: None,
                attestation: None,
                cause: None,
            },
            WORKSPACE_SCOPE,
        )?;
        let attestation = signed.attestation().context("fixture attestation")?;
        let receipt =
            store.client_workspace_receipt(&network, &client, "command", &command, attestation)?;
        assert_eq!(receipt["receipt"], "recorded");
        assert!(
            store
                .client_workspace_receipt(&network, &client, "another", &command, attestation)
                .is_err()
        );
        store
            .connection
            .execute("UPDATE events SET effective=0", [])?;
        let receipt =
            store.client_workspace_receipt(&network, &client, "command", &command, attestation)?;
        assert_eq!(receipt["receipt"], "not_seen_here");
        assert!(receipt.get("retry_safe").is_none());
        Ok(())
    }
}
