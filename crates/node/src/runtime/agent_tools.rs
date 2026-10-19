use super::*;
use crate::work::WorkSpec;

impl Runtime {
    pub(super) async fn agent_tool(
        self: &Arc<Self>,
        access: Access,
        name: &str,
        args: Value,
    ) -> Result<Value> {
        let scope = access
            .conversation_id
            .clone()
            .context("missing tool scope")?;
        let write = match name {
            "nodes" | "context" | "work_status" | "call_lookup" => false,
            "dispatch_work" | "interrupt_work" => true,
            _ => bail!("unknown or unavailable coordination tool"),
        };
        ensure!(
            !write || access.manager.is_some(),
            "work changes require a live agent invocation"
        );
        let _lease = tokio::time::timeout(
            Duration::from_secs(20),
            self.manager_tool_lease(&access, write),
        )
        .await
        .context("agent tool admission timed out")??;
        let network = access.network_id.as_str();
        match name {
            "nodes" => {
                self.rpc_inner(access.clone(), json!({"method":"nodes"}))
                    .await
            }
            "context" => self.agent_context(network, &scope, &args).await,
            "work_status" => {
                let works = self.store.lock().await.works(network, &scope)?;
                Ok(match args["work_id"].as_str() {
                    Some(id) => json!(
                        works
                            .into_iter()
                            .find(|work| work.work_id == id)
                            .context("work not found in this conversation")?
                    ),
                    None => json!({ "works": works }),
                })
            }
            "call_lookup" => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("call lookup requires an agent invocation")?;
                let id = WorkSpec::id(&scope, &manager.trigger, field(&args, "call_ref")?);
                let works = self.store.lock().await.works(network, &scope)?;
                Ok(json!(
                    works
                        .into_iter()
                        .find(|work| work.work_id == id)
                        .context("no work was dispatched with this call_ref")?
                ))
            }
            "dispatch_work" => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("dispatch requires an agent invocation")?;
                let call_ref = field(&args, "call_ref")?;
                vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
                let local = self.identity.device_id()?;
                let optional = |key: &str| args[key].as_str().map(String::from);
                let spec = WorkSpec {
                    work_id: WorkSpec::id(&scope, &manager.trigger, call_ref),
                    conversation: scope.clone(),
                    trigger: manager.trigger.clone(),
                    attempt: manager.attempt_id.clone(),
                    target: local.clone(),
                    dispatcher: local,
                    call_ref: call_ref.into(),
                    executor: crate::model::Executor::Codex,
                    cwd: field(&args, "cwd")?.into(),
                    prompt: field(&args, "prompt")?.into(),
                    continues: optional("continues"),
                    parent: optional("parent"),
                };
                let record = self.dispatch_work(network, spec).await?;
                Ok(json!({"work_id":record.spec.work_id,"call_ref":call_ref,"state":record.state}))
            }
            "interrupt_work" => {
                let id = field(&args, "work_id")?;
                ensure!(
                    self.store
                        .lock()
                        .await
                        .works(network, &scope)?
                        .iter()
                        .any(|work| work.work_id == id),
                    "work not found in this conversation"
                );
                self.interrupt_work(network, id).await
            }
            _ => unreachable!(),
        }
    }

    async fn agent_context(&self, network: &str, scope: &str, args: &Value) -> Result<Value> {
        let offset = args["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        let store = self.store.lock().await;
        if let Some(hash) = args["blob_hash"].as_str() {
            // ponytail: each page re-reads the whole file; blobs are capped at MAX_BLOB_BYTES.
            let (blob, bytes) = store.read_blob(network, scope, hash)?;
            ensure!(offset <= bytes.len(), "invalid blob offset");
            let end = offset.saturating_add(limit * 1024).min(bytes.len());
            return Ok(json!({"hash":blob.hash,"size":blob.size,"offset":offset,
                "data_hex":hex::encode(&bytes[offset..end]),"next_offset":(end<bytes.len()).then_some(end)}));
        }
        let events = store.context(network, scope, None)?.events;
        let end = offset.saturating_add(limit).min(events.len());
        ensure!(offset <= end, "invalid event offset");
        let page = events[offset..end]
            .iter()
            .map(|event| {
                json!({"author":event.event.author,"created_at_ms":event.event.created_at_ms,
                    "body":crate::scheduler::agent_event_body(&event.event.body),"blobs":event.event.blobs})
            })
            .collect::<Vec<_>>();
        Ok(json!({"events":page,"next_offset":(end<events.len()).then_some(end)}))
    }
}
