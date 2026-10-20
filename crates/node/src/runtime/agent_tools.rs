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
        "target_node":spec.target,"kind":spec.kind});
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
            "track_work" | "bind_work" | "dispatch_work" | "interrupt_work" => true,
            name if SETTINGS_TOOLS.contains(&name) => true,
            _ => bail!("unknown or unavailable coordination tool"),
        };
        ensure!(
            !write || access.manager.is_some(),
            "work changes require a live agent invocation"
        );
        let _lease =
            tokio::time::timeout(Duration::from_secs(20), self.manager_tool_lease(&access))
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
                    anyhow::anyhow!("{error}. Model, provider and folders change only in Settings")
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
            "track_work" => self.track_work(&access, &scope, &args).await,
            "bind_work" => self.bind_work(network, &scope, &args).await,
            // The Cursor SDK calls back instead of running the CLI; the same flow runs here.
            "dispatch_work" => {
                let socket = self.codex.socket.clone();
                let (access, scope) = (&access, scope.as_str());
                crate::agent_cli::dispatch(
                    &args,
                    |args| async move { self.track_work(access, scope, &args).await },
                    |args| async move { self.bind_work(network, scope, &args).await },
                    &socket,
                )
                .await
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

    /// Registers a dispatch before anything is sent. On this device it returns `may_send` only to
    /// the call that created the work; another device's assistant gets the task instead.
    async fn track_work(
        self: &Arc<Self>,
        access: &Access,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        let network = access.network_id.as_str();
        let manager = access
            .manager
            .as_ref()
            .context("dispatch requires an agent invocation")?;
        let call_ref = field(args, "call_ref")?;
        vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
        let local = self.identity.device_id()?;
        let optional = |key: &str| args[key].as_str().map(String::from);
        // An agent task's turn only starts work here, as children of its task.
        let task = manager.trigger.strip_prefix("task:");
        let target = match task {
            Some(_) => local.clone(),
            None => optional("target_node").unwrap_or_else(|| local.clone()),
        };
        let spec = WorkSpec {
            work_id: WorkSpec::id(scope, &manager.trigger, call_ref),
            conversation: scope.into(),
            trigger: manager.trigger.clone(),
            attempt: manager.attempt_id.clone(),
            target: target.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind: if target == local {
                WorkKind::Codex
            } else {
                WorkKind::Task
            },
            cwd: field(args, "cwd")?.into(),
            title: field(args, "title")?.chars().take(120).collect(),
            prompt: field(args, "prompt")?.into(),
            continues: optional("continues"),
            parent: task.map(String::from).or_else(|| optional("parent")),
        };
        if target != local {
            let dispatch = self.send_dispatch(network, spec).await?;
            return Ok(dispatch_value(&dispatch));
        }
        let (record, created) = self.accept_dispatch(network, spec).await?;
        let thread = match &record.spec.continues {
            Some(original) => self
                .store
                .lock()
                .await
                .works(network, scope)?
                .into_iter()
                .find(|work| &work.work_id == original)
                .and_then(|work| work.thread_id),
            None => None,
        };
        Ok(
            json!({"work_id":record.spec.work_id,"call_ref":call_ref,"state":record.state,
            "may_send":created,"send_by_ms":record.send_by_ms,"cwd":record.spec.cwd,"thread_id":thread}),
        )
    }
}
