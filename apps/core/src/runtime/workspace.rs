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
        if {
            let network_id = access.network_id.to_owned();
            let command_id = id(input, "command_id")?;
            self.store
                .call("workspace.workspace_command_recorded", move |store| {
                    store.workspace_command_recorded(&network_id, &command_id)
                })
                .await
        }? {
            return self
                .accept_workspace_command(&access.network_id, input)
                .await;
        }
        if let WorkspaceCommand::ControlInput { message_id, .. } = &command {
            let owner = {
                let (network, message_id) = (access.network_id.clone(), message_id.clone());
                self.store
                    .call("workspace.input_owner", move |store| {
                        let input = store
                            .workspace_input(&network, &message_id)?
                            .context("input not found")?;
                        Ok(store
                            .resolve_input_target(&network, &input.input.route.target)?
                            .owner_node)
                    })
                    .await?
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
        let result = {
            let identity = self.identity.to_owned();
            let network = network.to_owned();
            let command_id = command_id.to_owned();
            let signature = Some(attestation(input)?);
            self.store
                .call("workspace.apply_workspace_command", move |store| {
                    store.apply_workspace_command(
                        &identity,
                        &network,
                        &command_id,
                        command,
                        signature,
                        None,
                        None,
                        now_ms(),
                    )
                })
                .await
        };
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
        Ok(serde_json::to_value({
            let network_id = access.network_id.to_owned();
            let channel = channel.clone();
            let task = task.clone();
            self.read_display(access, input, "workspace.workspace_state", move |store| {
                store.workspace_state(&network_id, channel.as_deref(), task.as_deref(), limit)
            })
            .await
        }?)?)
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
        let (events, truncated) = {
            let network_id = access.network_id.to_owned();
            let channel = channel.clone();
            let task = task.clone();
            self.read_display(access, input, "workspace.workspace_history", move |store| {
                store.workspace_history(&network_id, channel.as_deref(), task.as_deref(), limit)
            })
            .await
        }?;
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
            let network = node.store.call("workspace.network", move |store| store.network()).await?.context("network")?;
            let bot = {
                let network = network.to_owned();
                node.store.call("workspace.bot_for_conversation", move |store| store.bot_for_conversation(&network, "c")).await
            }?.context("bot")?;
            let request = InputRequest {
                message_id: "deferred".into(),
                route: InputRoute {
                    target: InputTarget::BotMain { bot_id: bot.bot_id },
                    intent: InputIntent::Defer, sequence: 1, expected_revision: None,
                },
                text: "saved input".into(), attachments: vec![], report_results: true,
            };
            let signed = admin.attest(&network, "c", EventBody::InputPosted {
                message_id: request.message_id.clone(), target: request.route.target.clone(),
                intent: request.route.intent, sequence: request.route.sequence,
                expected_revision: None, text: request.text.clone(), attachments: vec![],
                report_results: Some(true), attestation: None,
            })?;
            let original = {
                let identity = node.identity.to_owned();
                let network = network.to_owned();
                let signature = signed.attestation().cloned().context("signature")?;
                node.store.call("workspace.post_workspace_input", move |store| store.post_workspace_input(&identity, &network, request, signature, now_ms())).await
            }?;
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
            let before = {
                let network = network.to_owned();
                node.store.call("workspace.version_vector", move |store| store.version_vector(&network)).await
            }?;
            let stale = control("stale-release", &crate::crypto::digest(b"other control"))?;
            let rejected = node.workspace_command(&access, &stale).await?;
            assert_eq!(rejected, json!({"accepted":false,"stored":false,"code":"workspace_command_not_saved",
                "command_id":"stale-release","reason":"input control version changed"}));
            {
                let (network, before, source_event) = (network.clone(), before.clone(), original.source_event.clone());
                node.store.call("workspace.assert_rejected", move |store| {
                    assert!(!store.workspace_command_recorded(&network, "stale-release")?);
                    assert_eq!(store.version_vector(&network)?, before);
                    assert_eq!(store.queued_workspace_input(&network, "deferred")?.context("input")?.control_event, source_event);
                    Ok(())
                }).await?;
            }
            let valid = control("valid-release", &original.source_event)?;
            node.store.call("workspace.connection.pragma_update", move |store| Ok(store.connection.pragma_update(None, "query_only", true)?)).await?;
            let io_error = node.workspace_command(&access, &valid).await.expect_err("database write failure remains an RPC error");
            node.store.call("workspace.connection.pragma_update", move |store| Ok(store.connection.pragma_update(None, "query_only", false)?)).await?;
            assert!(io_error.downcast_ref::<WorkspaceCommandNotSaved>().is_none());
            assert!(!{
                let network = network.to_owned();
                node.store.call("workspace.workspace_command_recorded", move |store| store.workspace_command_recorded(&network, "valid-release")).await
            }?);
            assert_eq!({
                let network = network.to_owned();
                node.store.call("workspace.version_vector", move |store| store.version_vector(&network)).await
            }?, before);
            let applied = node.workspace_command(&access, &valid).await?;
            assert_eq!(applied["command_id"], "valid-release");
            assert_eq!(applied["duplicate"], false);
            assert!(applied["event_hash"].as_str().is_some_and(|hash| !hash.is_empty()));
            let committed = {
                let network = network.to_owned();
                node.store.call("workspace.version_vector", move |store| store.version_vector(&network)).await
            }?;
            let duplicate = node.workspace_command(&access, &valid).await?;
            assert_eq!(duplicate["duplicate"], true);
            assert_eq!(duplicate["event_hash"], applied["event_hash"]);
            assert_eq!({
                let network = network.to_owned();
                node.store.call("workspace.version_vector", move |store| store.version_vector(&network)).await
            }?, committed);
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
