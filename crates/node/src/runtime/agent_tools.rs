use super::*;
use crate::{
    scheduler::SETTINGS_TOOLS,
    store::assistant::{ChangeRequest, change_id},
    work::{Dispatch, DispatchState, WorkSpec},
};

/// A dispatch to another device that has not shown up as work yet.
fn dispatch_value(dispatch: &Dispatch) -> Value {
    let spec = &dispatch.spec;
    let mut value = json!({"work_id":spec.work_id,"call_ref":spec.call_ref,
        "target_node":spec.target,"executor":spec.executor});
    match &dispatch.state {
        DispatchState::Pending => value["state"] = json!("waiting_for_device"),
        DispatchState::Accepted => value["state"] = json!("accepted"),
        DispatchState::Rejected { reason } => {
            value["state"] = json!("rejected");
            value["reason"] = json!(reason);
        }
    }
    value
}

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
            "nodes" | "context" | "work_status" | "call_lookup" | "read_skill" => false,
            "dispatch_work" | "interrupt_work" => true,
            name if SETTINGS_TOOLS.contains(&name) => true,
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
                let call_ref = field(&args, "call_ref")?;
                let id = WorkSpec::id(&scope, &manager.trigger, call_ref);
                let store = self.store.lock().await;
                if let Some(work) = store
                    .works(network, &scope)?
                    .into_iter()
                    .find(|work| work.work_id == id)
                {
                    return Ok(json!(work));
                }
                if let Some(dispatch) = store.dispatch(network, &id)? {
                    return Ok(dispatch_value(&dispatch));
                }
                let change = store
                    .assistant_change(network, &change_id(&scope, &manager.trigger, call_ref))?
                    .context("nothing was dispatched or changed with this call_ref")?;
                Ok(json!(change))
            }
            "read_skill" => {
                let id = field(&args, "skill_id")?;
                let skill = self
                    .store
                    .lock()
                    .await
                    .skill(network, id)?
                    .with_context(|| format!("skill {id} not found"))?;
                let mut value = json!(skill);
                value["skill_id"] = json!(id);
                Ok(value)
            }
            name if SETTINGS_TOOLS.contains(&name) => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("assistant changes require an agent invocation")?;
                let call_ref = field(&args, "call_ref")?.to_owned();
                let mut request = args;
                let fields = request
                    .as_object_mut()
                    .context("tool arguments must be an object")?;
                fields.remove("call_ref");
                fields.insert("tool".into(), json!(name));
                let request: ChangeRequest = serde_json::from_value(request).map_err(|error| {
                    anyhow::anyhow!(
                        "{error}. Model, provider, folders and autonomy change only in Settings"
                    )
                })?;
                let (record, written) = self.store.lock().await.change_assistant(
                    &self.identity,
                    network,
                    &scope,
                    &manager.trigger,
                    &call_ref,
                    request,
                    now_ms(),
                )?;
                if written {
                    self.publish_changed(network, &scope, &record.event_hash);
                }
                Ok(json!(record))
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
                // An agent task's turn only runs Codex here, as children of its task.
                let task = manager.trigger.strip_prefix("task:");
                let target = match task {
                    Some(_) => local.clone(),
                    None => optional("target_node").unwrap_or_else(|| local.clone()),
                };
                let executor = match (task, args["executor"].as_str()) {
                    (_, None | Some("codex")) => Executor::Codex,
                    (None, Some("agent")) => Executor::Agent,
                    _ => bail!("an agent task only starts Codex work on its own device"),
                };
                let spec = WorkSpec {
                    work_id: WorkSpec::id(&scope, &manager.trigger, call_ref),
                    conversation: scope.clone(),
                    trigger: manager.trigger.clone(),
                    attempt: manager.attempt_id.clone(),
                    target: target.clone(),
                    dispatcher: local.clone(),
                    call_ref: call_ref.into(),
                    executor,
                    cwd: field(&args, "cwd")?.into(),
                    title: field(&args, "title")?.chars().take(120).collect(),
                    prompt: field(&args, "prompt")?.into(),
                    continues: optional("continues"),
                    parent: task.map(String::from).or_else(|| optional("parent")),
                };
                if target == local {
                    let record = self.accept_dispatch(network, spec).await?;
                    return Ok(
                        json!({"work_id":record.spec.work_id,"call_ref":call_ref,"state":record.state}),
                    );
                }
                let dispatch = self.send_dispatch(network, spec).await?;
                Ok(dispatch_value(&dispatch))
            }
            "interrupt_work" => {
                let id = field(&args, "work_id")?;
                self.stop_work(network, &scope, id).await
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
