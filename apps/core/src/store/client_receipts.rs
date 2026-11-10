//! Positive evidence for a native client's exact signed command. Absence never permits replay.
use super::*;
use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand};
use serde_json::json;

impl Store {
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
        if scope==WORKSPACE_SCOPE {
            let bot=client.attest(&network,"chat",EventBody::BotCreated {bot_id:"bot".into(),name:"Fixture".into(),instructions:String::new(),project:None,attestation:None,cause:None})?;
            store.create_bot(&node,&network,&BotCreateSpec {bot_id:"bot".into(),conversation_id:"chat".into(),name:"Fixture".into(),instructions:String::new(),project:None,attestation:bot.attestation().context("fixture bot signature")?.clone(),owner:node.owner_fixture(&network,"chat",&node.device_id()?)?},1)?;
        }
        store.append_event(&node, &network, scope, signed.clone(), vec![], 2)?;
        Ok((store, network, client.device_id()?, signed))
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
