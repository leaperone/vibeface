use super::*;
use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand, WorkspaceCommandNotSaved};

impl Runtime {
    pub(super) async fn workspace_command(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.human && access.conversation_id.is_none(),
            "workspace commands need the user's personal network access"
        );
        ensure!(
            input.get("cause").is_none() && input.get("acting_assignment").is_none(),
            "clients cannot issue Bot causes"
        );
        let command: WorkspaceCommand = serde_json::from_value(input["command"].clone())
            .context("invalid workspace command")?;
        if self
            .store
            .lock()
            .await
            .workspace_command_recorded(&access.network_id, &id(input, "command_id")?)?
        {
            return self
                .accept_workspace_command(&access.network_id, input)
                .await;
        }
        if let WorkspaceCommand::ControlInput { message_id, .. } = &command {
            let owner = {
                let store = self.store.lock().await;
                let input = store
                    .workspace_input(&access.network_id, message_id)?
                    .context("input not found")?;
                store
                    .resolve_input_target(&access.network_id, &input.input.route.target)?
                    .owner_node
            };
            if owner != self.identity.device_id()? {
                return self.peer_request(&access.network_id, &owner, json!({"type":"workspace_command","command_id":id(input,"command_id")?,"command":command,"attestation":attestation(input)?})).await;
            }
        }
        self.accept_workspace_command(&access.network_id, input)
            .await
    }
    pub(super) async fn accept_workspace_command(
        &self,
        network: &str,
        input: &Value,
    ) -> Result<Value> {
        ensure!(
            input.get("cause").is_none() && input.get("acting_assignment").is_none(),
            "a peer cannot manufacture a Bot cause"
        );
        let command: WorkspaceCommand = serde_json::from_value(input["command"].clone())
            .context("invalid workspace command")?;
        let command_id = id(input, "command_id")?;
        let result = self.store.lock().await.apply_workspace_command(
            &self.identity,
            network,
            &command_id,
            command,
            Some(attestation(input)?),
            None,
            None,
            now_ms(),
        );
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Some(rejection) = error.downcast_ref::<WorkspaceCommandNotSaved>() {
                    ensure!(
                        rejection.command_id == command_id,
                        "workspace command rejection identity mismatch"
                    );
                    return Ok(
                        json!({"accepted":false,"stored":false,"code":"workspace_command_not_saved",
                        "command_id":rejection.command_id,"reason":rejection.reason}),
                    );
                }
                return Err(error);
            }
        };
        self.publish_changed(
            network,
            WORKSPACE_SCOPE,
            Changed::Event(&receipt.event_hash),
        );
        self.schedule_wakeup.notify_one();
        Ok(serde_json::to_value(receipt)?)
    }
    pub(super) async fn workspace_state(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.human && access.conversation_id.is_none(),
            "workspace reads need the user's personal network access"
        );
        let channel = input
            .get("channel_id")
            .map(|_| id(input, "channel_id"))
            .transpose()?;
        let task = input
            .get("task_id")
            .map(|_| id(input, "task_id"))
            .transpose()?;
        let limit = domain_limit(input)?;
        Ok(serde_json::to_value(
            self.store.lock().await.workspace_state(
                &access.network_id,
                channel.as_deref(),
                task.as_deref(),
                limit,
            )?,
        )?)
    }
    pub(super) async fn workspace_history(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.human && access.conversation_id.is_none(),
            "workspace reads need the user's personal network access"
        );
        let channel = input
            .get("channel_id")
            .map(|_| id(input, "channel_id"))
            .transpose()?;
        let task = input
            .get("task_id")
            .map(|_| id(input, "task_id"))
            .transpose()?;
        let limit = domain_limit(input)?;
        let (events, truncated) = self.store.lock().await.workspace_history(
            &access.network_id,
            channel.as_deref(),
            task.as_deref(),
            limit,
        )?;
        Ok(
            json!({"events":events,"coverage":{"limit":limit,"truncated":truncated,"scope":if task.is_some(){"task"}else if channel.is_some(){"channel"}else{"workspace"}}}),
        )
    }
}
fn domain_limit(input: &Value) -> Result<usize> {
    let limit = input
        .get("limit")
        .map(|value| value.as_u64().context("invalid workspace limit"))
        .transpose()?
        .unwrap_or(256);
    ensure!((1..=512).contains(&limit), "workspace limit must be 1-512");
    Ok(limit as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::access as test_access;
    use crate::{
        model::EventBody,
        runtime::triage::tests::{bot, node},
        workspace::{InputControl, InputIntent, InputRequest, InputRoute, InputTarget},
    };

    #[tokio::test]
    async fn only_preappend_control_rejections_return_a_command_not_saved_receipt() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let result = async {
            bot(&node, &admin, "c", "Workspace", &[]).await?;
            let network = node.store.lock().await.network()?.context("network")?;
            let bot = node.store.lock().await.bot_for_conversation(&network, "c")?.context("bot")?;
            let request = InputRequest {
                message_id: "deferred".into(),
                route: InputRoute {
                    target: InputTarget::BotMain { bot_id: bot.bot_id },
                    intent: InputIntent::Defer, sequence: 1, expected_revision: None,
                },
                text: "saved input".into(), attachments: vec![], report_results: Some(true),
            };
            let signed = admin.attest(&network, "c", EventBody::InputPosted {
                message_id: request.message_id.clone(), target: request.route.target.clone(),
                intent: request.route.intent, sequence: request.route.sequence,
                expected_revision: None, text: request.text.clone(), attachments: vec![],
                report_results: Some(true), attestation: None,
            })?;
            let original = node.store.lock().await.post_workspace_input(
                &node.identity, &network, request, signed.attestation().cloned().context("signature")?, now_ms(),
            )?;
            let control = |command_id: &str, expected_event: &str| -> Result<Value> {
                let command = WorkspaceCommand::ControlInput {
                    message_id: "deferred".into(), expected_event: expected_event.into(), action: InputControl::Release,
                };
                let signed = admin.attest(&network, WORKSPACE_SCOPE, EventBody::WorkspaceCommanded {
                    command_id: command_id.into(), command: command.clone(), attestation: None,
                    cause: None, acting_assignment: None,
                })?;
                Ok(json!({"command_id":command_id,"command":command,"attestation":signed.attestation().context("control signature")?}))
            };
            let access = test_access(network.clone(), true, None);
            let before = node.store.lock().await.version_vector(&network)?;
            let stale = control("stale-release", &crate::crypto::digest(b"other control"))?;
            let rejected = node.workspace_command(&access, &stale).await?;
            assert_eq!(rejected, json!({"accepted":false,"stored":false,"code":"workspace_command_not_saved",
                "command_id":"stale-release","reason":"input control version changed"}));
            {
                let store = node.store.lock().await;
                assert!(!store.workspace_command_recorded(&network, "stale-release")?);
                assert_eq!(store.version_vector(&network)?, before);
                assert_eq!(store.queued_workspace_input(&network, "deferred")?.context("input")?.control_event, original.source_event);
            }
            let valid = control("valid-release", &original.source_event)?;
            node.store.lock().await.connection.pragma_update(None, "query_only", true)?;
            let io_error = node.workspace_command(&access, &valid).await.expect_err("database write failure remains an RPC error");
            node.store.lock().await.connection.pragma_update(None, "query_only", false)?;
            assert!(io_error.downcast_ref::<WorkspaceCommandNotSaved>().is_none());
            assert!(!node.store.lock().await.workspace_command_recorded(&network, "valid-release")?);
            assert_eq!(node.store.lock().await.version_vector(&network)?, before);
            let applied = node.workspace_command(&access, &valid).await?;
            assert_eq!(applied["command_id"], "valid-release");
            assert_eq!(applied["duplicate"], false);
            assert!(applied["event_hash"].as_str().is_some_and(|hash| !hash.is_empty()));
            let committed = node.store.lock().await.version_vector(&network)?;
            let duplicate = node.workspace_command(&access, &valid).await?;
            assert_eq!(duplicate["duplicate"], true);
            assert_eq!(duplicate["event_hash"], applied["event_hash"]);
            assert_eq!(node.store.lock().await.version_vector(&network)?, committed);
            Ok::<_, anyhow::Error>(())
        }.await;
        let socket = node
            .providers()
            .switches
            .codex_app_server
            .socket
            .context("fixture socket")?;
        let _ = std::fs::remove_file(socket);
        drop(node);
        std::fs::remove_dir_all(directory)?;
        result
    }
}
