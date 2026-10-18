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
        spec.validate()?;
        spec.cwd = self.allowed_path(
            spec.cwd
                .to_str()
                .context("command directory must be UTF-8")?,
        )?;
        Ok(spec)
    }

    pub(super) async fn execute_command(self: &Arc<Self>, request: OperationRequest) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let preparation: Result<(CommandSpec, ExecutionContext)> = async {
            self.require_role(&request.network_id, NodeRole::Executor)
                .await?;
            let spec = self.command_spec(&request.action)?;
            for resource in &request.resources {
                self.allowed_path(resource)?;
            }
            ensure!(
                request.task_id.is_none()
                    && request.session_id.is_none()
                    && request.binding.is_none()
                    && request.expected_revision == 0,
                "command must not invent a Task, session or binding"
            );
            let authorization = self.store.lock().await.operation_authorization(
                &request.network_id,
                &request.operation_id,
                now_ms() as u64,
            )?;
            let freshness = self.freshness(&authorization, CheckStage::Start).await?;
            let mut store = self.store.lock().await;
            store.validate_operation_binding(&request)?;
            let (_, deadline) = store.claim_authorized_operation(
                &request.network_id,
                &request.operation_id,
                &freshness,
                now_ms() as u64,
            )?;
            let context = execution_context(&store, &request, deadline)?;
            Ok((spec, context))
        }
        .await;
        let (spec, context) = match preparation {
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
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let execution = crate::command::execute(&spec, cancelled);
        tokio::pin!(execution);
        let mut renewal = tokio::time::interval(Duration::from_secs(5));
        renewal.tick().await;
        let outcome = loop {
            tokio::select! {
                result = &mut execution => break result,
                _ = renewal.tick() => {
                    let renew: Result<()> = async {
                        let authorization = self.store.lock().await.operation_authorization(
                            context.space_id.as_str(), &context.operation_id, now_ms() as u64)?;
                        let proof = self.freshness(&authorization, CheckStage::Renew).await?;
                        self.store.lock().await.renew_authorized_operation(
                            context.space_id.as_str(), &context.operation_id, &proof, now_ms() as u64)?;
                        Ok(())
                    }.await;
                    if renew.is_err() {
                        let _ = cancel.send(true);
                        break execution.await;
                    }
                }
            }
        };
        match outcome {
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
