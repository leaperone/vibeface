use super::*;
use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand, WorkspaceCommandNotSaved};

impl Runtime {
    pub(super) async fn workspace_command(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.is_user() && access.conversation_id().is_none(),
            "workspace commands need the user's personal network access"
        );
        ensure!(
            input.get("cause").is_none() && input.get("acting_assignment").is_none(),
            "clients cannot issue Bot causes"
        );
        let command: WorkspaceCommand = serde_json::from_value(input["command"].clone())
            .context("invalid workspace command")?;
        if {
            let network_id = access.network_id().to_owned();
            let command_id = id(input, "command_id")?;
            self.store
                .call("workspace.workspace_command_recorded", move |store| {
                    store.workspace_command_recorded(&network_id, &command_id)
                })
                .await
        }? {
            return self
                .accept_workspace_command(access.network_id(), input)
                .await;
        }
        if let WorkspaceCommand::ControlInput { message_id, .. } = &command {
            let owner = {
                let (network, message_id) = (access.network_id().to_owned(), message_id.clone());
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
                return self.peer_request(access.network_id(), &owner, json!({"type":"workspace_command","command_id":id(input,"command_id")?,"command":command,"attestation":attestation(input)?})).await;
            }
        }
        self.accept_workspace_command(access.network_id(), input)
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
            access.is_user() && access.conversation_id().is_none(),
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
            let network_id = access.network_id().to_owned();
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
            access.is_user() && access.conversation_id().is_none(),
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
        let cursor = input
            .get("cursor")
            .map(|value| {
                value
                    .as_str()
                    .context("invalid history cursor")
                    .map(str::to_owned)
            })
            .transpose()?;
        let network_id = access.network_id().to_owned();
        Ok(serde_json::to_value(
            self.read_display(access, input, "workspace.workspace_history", move |store| {
                store.workspace_history(
                    &network_id,
                    channel.as_deref(),
                    task.as_deref(),
                    cursor.as_deref(),
                    limit,
                )
            })
            .await?,
        )?)
    }
    pub(super) async fn message_search(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.is_user() && access.conversation_id().is_none(),
            "message search needs the user's personal network access"
        );
        let mut fields = input.clone();
        if let Some(fields) = fields.as_object_mut() {
            fields.remove("method");
            fields.remove("client_id");
        }
        let request: vibeface_protocol::message_search::MessageSearchRequest =
            serde_json::from_value(fields).context("invalid message search request")?;
        let network = access.network_id().to_owned();
        Ok(serde_json::to_value(
            self.read_display(access, input, "workspace.message_search", move |store| {
                store.message_search(&network, &request)
            })
            .await?,
        )?)
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
    use crate::{
        model::EventBody,
        runtime::test_support::{bot, node},
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
            let signed = admin.attest(&network, "c", crate::workspace::input_event_body(&request, None))?;
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
            let access = crate::runtime::Access::User { network_id: network.clone(), scope: crate::runtime::UserScope::Network };
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
    #[tokio::test]
    async fn message_search_dispatch_is_read_only_and_rejects_agent_and_scoped_user_access()
    -> Result<()> {
        let (node, admin, directory) = node().await?;
        let result = async {
            bot(&node, &admin, "c", "Search", &[]).await?;
            let network = node.store.call("search.network", |store| store.network()).await?.context("network")?;
            let bot_id = {
                let network = network.clone();
                node.store.call("search.bot", move |store| Ok(store.bot_for_conversation(&network, "c")?.context("Bot")?.bot_id)).await?
            };
            let before = {
                let network = network.clone();
                node.store.call("search.before", move |store| store.version_vector(&network)).await?
            };
            let request = json!({"method":"message_search","scope":{"kind":"bot_main","bot_id":bot_id},"query":"中文"});
            let user = Access::User { network_id: network.clone(), scope: UserScope::Network };
            let page = node.rpc(user.clone(), crate::command::business_fixture(request.clone())).await?;
            assert_eq!(page["scope"], request["scope"]);
            assert_eq!(page["query"], "中文");
            assert_eq!(page["items"], json!([]));
            assert_eq!(page["complete"], true);
            assert_eq!(page["coverage"]["source"], "committed_server_history");
            for scope in [json!({"kind":"bot_main","bot_id":"other"}), json!({"kind":"task","task_id":"other"}), json!({"kind":"channel","channel_id":"other"})] {
                let request = json!({"method":"message_search","scope":scope,"query":"中文"});
                for access in [
                    Access::Agent(AgentAccess::ReadOnly { network_id: network.clone(), conversation_id: "c".into() }),
                    Access::User { network_id: network.clone(), scope: UserScope::Conversation("c".into()) },
                ] {
                    let error = node.rpc(access, crate::command::business_fixture(request.clone())).await.expect_err("scoped access cannot use personal search");
                    assert!(error.to_string().contains("Agent tokens") || error.to_string().contains("personal network access"), "{error:#}");
                }
            }
            for extra in [json!({"conversation_id":"c"}), json!({"unknown":true}), json!({"scope":{"kind":"bot_main","bot_id":bot_id,"task_id":"other"}})] {
                let mut invalid = request.clone();
                invalid.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                assert!(node.rpc(user.clone(), crate::command::business_fixture(invalid)).await.is_err());
            }
            let after = {
                let network = network.clone();
                node.store.call("search.after", move |store| store.version_vector(&network)).await?
            };
            assert_eq!(before, after);
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
