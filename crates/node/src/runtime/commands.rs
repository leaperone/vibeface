use super::*;
use crate::command::CommandSpec;

impl Runtime {
    pub(super) fn command_spec(&self, action: &Value) -> Result<CommandSpec> {
        let mut spec = action.clone();
        let fields = spec.as_object_mut().context("invalid command action")?;
        ensure!(
            fields.remove("type") == Some(json!("command")),
            "invalid command type"
        );
        let mut spec: CommandSpec = serde_json::from_value(spec)?;
        spec.cwd = self.allowed_path(
            spec.cwd
                .to_str()
                .context("command directory must be UTF-8")?,
        )?;
        Ok(spec)
    }

    pub(super) async fn execute_command(self: &Arc<Self>, request: OperationRequest) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let preparation: Result<CommandSpec> = async {
            self.require_role(&request.network_id, NodeRole::Executor)
                .await?;
            let spec = self.command_spec(&request.action)?;
            for resource in &request.resources {
                self.allowed_path(resource)?;
            }
            let mut store = self.store.lock().await;
            store.validate_operation_binding(&request)?;
            store.claim_operation(&request.network_id, &request.operation_id)?;
            Ok(spec)
        }
        .await;
        let spec = match preparation {
            Ok(spec) => spec,
            Err(error) => {
                if self
                    .store
                    .lock()
                    .await
                    .operation(&request.network_id, &request.operation_id)?
                    .is_some_and(|record| record.state == OperationState::Accepted)
                {
                    self.finish(
                        &request,
                        OperationState::Rejected,
                        json!({"error":format!("{error:#}")}),
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        let (_cancel, cancelled) = tokio::sync::watch::channel(false);
        match crate::command::execute(&spec, cancelled).await {
            Ok(output) => {
                let state = if output.exit_code == Some(0) {
                    OperationState::Succeeded
                } else {
                    OperationState::Failed
                };
                self.finish(&request, state, serde_json::to_value(output)?)
                    .await?;
            }
            Err(error) => {
                let state = if error.uncertain {
                    OperationState::Unknown
                } else {
                    OperationState::Failed
                };
                self.finish(&request, state, json!({"error":error.message}))
                    .await?;
            }
        }
        Ok(())
    }
}
