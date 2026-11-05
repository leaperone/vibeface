use super::*;
use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand};

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
        let receipt = self.store.lock().await.apply_workspace_command(
            &self.identity,
            network,
            &id(input, "command_id")?,
            command,
            Some(attestation(input)?),
            None,
            None,
            now_ms(),
        )?;
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
