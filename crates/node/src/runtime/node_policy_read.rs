use super::*;
use crate::{
    binding::SignedBinding,
    identity::{
        foreign_execution_policy::{PendingNodeTaskRead, task_policy_candidates_in},
        node_execution_policy::{NodeReadRequest, NodeReadResponse},
    },
};
use vibeface_secure::authorization::{
    Action, AuthorizationError, EventRef, InvocationContext, RequestFacts, ResourceKind,
    SessionRef,
    prepare::PrepareContext,
    remote_execution_policy::{
        RemoteExecutionPolicyPageRequest, RemoteExecutionPolicyPurpose,
        RemoteExecutionPolicySelection,
    },
};

struct SdkTaskPolicyInput {
    parent: VerifiedAuthorization,
    facts: RequestFacts,
    selection: RemoteExecutionPolicySelection,
    context: PrepareContext,
    binding: Option<SignedBinding>,
}

enum SdkTaskPolicyRead {
    Primary,
    Cached(VerifiedTaskCurrentPolicy),
    Pending(PendingNodeTaskRead),
}

fn policy_sync_needed(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<AuthorizationError>(),
        Some(AuthorizationError::NeedPolicySync(_))
    ) || error.to_string().starts_with("NeedPolicySync:")
}

impl Runtime {
    pub(super) fn bound_sdk_execution_tool_targets(
        &self,
        store: &Store,
        session: &AgentSession,
        task_id: &str,
        executor: &str,
        now: u64,
    ) -> Result<Vec<Value>> {
        let audience = self.audience()?;
        let saved = store
            .local_value(
                session.space_id.as_str(),
                &format!("agent-session:{}", session.sdk_session.session_id),
            )?
            .context("target discovery has no persisted SDK session")?;
        ensure!(
            saved == serde_json::to_value(session)?
                && now < session.expires_at_ms
                && session.sdk_session.target == session.device
                && audience.service == session.service
                && audience.device == session.device,
            "target discovery changed its current SDK delegation"
        );
        let primary = store.authorization_state();
        let host = primary.verify_service_device(&session.space_id, &session.device, now)?;
        let project = primary
            .resource(&session.space_id, &session.resource_id)
            .context("target discovery has no current Project")?;
        ensure!(
            project.payload.kind == ResourceKind::Project
                && project.payload.host == session.service
                && host.payload.subject == Actor::Service(session.service.clone()),
            "target discovery changed the current Project host"
        );
        let origin = store.agent_origin(session, now)?;
        let creation =
            store.task_creation(session.space_id.as_str(), &session.conversation_id, task_id)?;
        let event = store.verify_event(session.space_id.as_str(), &creation)?;
        ensure!(
            event.resource_id == session.resource_id && event.scope_id == session.conversation_id,
            "target discovery belongs to another Project or conversation"
        );
        let task = EventRef::parse(&creation.hash)?;
        let binding = store.task_binding(session.space_id.as_str(), &creation.hash)?;
        if let Some((own_task, own_ref)) = &origin.task {
            ensure!(
                own_task == task_id && own_ref == &task,
                "continuation target discovery escaped its own Task"
            );
        } else {
            let EventCause::Invocation { admission } = &event.cause else {
                bail!("SDK Task has no admitted creation");
            };
            let own_sdk = admission.invocation.payload.actor
                == Actor::Service(session.service.clone())
                && admission.invocation.payload.context.sdk_session.as_ref()
                    == Some(&session.sdk_session)
                && admission.source_body["parent_invocation"].as_str()
                    == Some(session.parent_invocation.as_str());
            let account_bound = matches!(admission.invocation.payload.actor, Actor::Account(_))
                && admission.invocation.payload.actor == session.initiator
                && binding.is_some();
            ensure!(
                own_sdk || account_bound,
                "initial SDK discovery requires its own task or its initiator's confirmed binding"
            );
        }
        let Some(binding) = binding else {
            return Ok(vec![]);
        };
        store.verify_historical_binding(&binding)?;
        ensure!(
            binding.binding.open_spec["executor"].as_str() == Some(executor),
            "target discovery changed its confirmed executor"
        );
        if let Some(original) = &origin.execution {
            ensure!(
                original.binding.as_ref() == Some(&binding)
                    && original.target_node == binding.binding.target_node
                    && original.session_id == binding.binding.session_id,
                "SDK continuation changed its original execution binding"
            );
        }
        let (project, selection, context) =
            store.bound_node_task_policy_selection(&self.identity, &binding)?;
        ensure!(
            project.space == session.space_id
                && project.resource == session.resource_id
                && context.context.task.as_ref() == Some(&task)
                && context.context.conversation.as_deref()
                    == Some(session.conversation_id.as_str()),
            "bound target selection changed its current SDK Project or Task"
        );
        Ok(vec![json!({
            "execution_resource":selection.execution_resource,
            "target_public_key":selection.target_public_key,
            "execution_grant":selection.execution_grant,
            "target_node":binding.binding.target_node,
            "session_id":binding.binding.session_id,
            "executor":executor,
            "task_id":task_id,
            "task_ref":task,
            "cwd":context.canonical_cwd,
            "binding_status":"bound",
            "connection_status":"verification_required",
        })])
    }

    fn sdk_task_policy_input(
        &self,
        store: &Store,
        session: &AgentSession,
        body: &Value,
        now: u64,
    ) -> Result<SdkTaskPolicyInput> {
        let project = ResourceRef {
            space: session.space_id.clone(),
            resource: session.resource_id.clone(),
        };
        let audience = self.audience()?;
        ensure!(
            session.service == audience.service
                && session.device == audience.device
                && body["space_id"].as_str() == Some(project.space.as_str())
                && body["resource_id"].as_str() == Some(project.resource.as_str())
                && body["conversation_id"].as_str() == Some(session.conversation_id.as_str())
                && body["sdk_session"] == serde_json::to_value(&session.sdk_session)?
                && body["parent_invocation"].as_str() == Some(session.parent_invocation.as_str())
                && now < session.expires_at_ms,
            "SDK policy refresh changed its admitted service session"
        );
        let primary = store.authorization_state();
        let resource = primary
            .resource(&project.space, &project.resource)
            .context("SDK policy refresh has no current Project")?;
        let device = primary.verify_service_device(&project.space, &session.device, now)?;
        ensure!(
            resource.payload.kind == ResourceKind::Project
                && resource.payload.host == session.service
                && device.payload.subject == Actor::Service(session.service.clone())
                && device.payload.public_key.as_str() == self.identity.public_key_hex()?,
            "SDK policy refresh is not this Project host"
        );
        let origin = store.agent_origin(session, now)?;
        let request = if body["method"] == "operate" {
            Some(serde_json::from_value::<OperationRequest>(
                body["request"].clone(),
            )?)
        } else {
            ensure!(
                body["method"] == "bind_execution",
                "SDK policy refresh requires an execution request"
            );
            None
        };
        let action = authorization_action(body)?;
        ensure!(
            matches!(
                action,
                Action::ExecutionOpen
                    | Action::ExecutionSend
                    | Action::ExecutionSteer
                    | Action::ExecutionInterrupt
                    | Action::ExecutionRespondApproval
            ),
            "SDK policy refresh cannot rebind or resolve execution"
        );
        let task_id = request
            .as_ref()
            .and_then(|request| request.task_id.as_deref())
            .or_else(|| body["task_id"].as_str())
            .context("SDK execution has no Task")?;
        let creation =
            store.task_creation(project.space.as_str(), &session.conversation_id, task_id)?;
        let event = store.verify_event(project.space.as_str(), &creation)?;
        ensure!(
            event.resource_id == project.resource
                && event.scope_id == session.conversation_id
                && body["task_ref"]
                    .as_str()
                    .is_none_or(|reference| reference == creation.hash),
            "SDK policy refresh changed its ready Project Task"
        );
        let task = EventRef::parse(&creation.hash)?;
        let binding = store.task_binding(project.space.as_str(), &creation.hash)?;
        if let Some((own_task, own_ref)) = &origin.task {
            ensure!(
                own_task == task_id && own_ref == &task,
                "SDK policy refresh escaped its continuation Task"
            );
        } else {
            let EventCause::Invocation { admission } = &event.cause else {
                bail!("SDK policy refresh has no admitted Task creation");
            };
            let own_sdk = admission.invocation.payload.actor
                == Actor::Service(session.service.clone())
                && admission.invocation.payload.context.sdk_session.as_ref()
                    == Some(&session.sdk_session)
                && admission.source_body["parent_invocation"].as_str()
                    == Some(session.parent_invocation.as_str());
            let account_bound = matches!(admission.invocation.payload.actor, Actor::Account(_))
                && admission.invocation.payload.actor == session.initiator
                && binding.is_some();
            ensure!(
                own_sdk || account_bound,
                "SDK policy refresh requires its own Task or its initiator's confirmed binding"
            );
        }
        let selection = RemoteExecutionPolicySelection {
            execution_resource: serde_json::from_value(body["execution_resource"].clone())?,
            target_public_key: serde_json::from_value(body["target_public_key"].clone())?,
            execution_grant: serde_json::from_value(body["execution_grant"].clone())?,
        };
        let target = crypto::fingerprint(selection.target_public_key.as_str())?;
        let (execution_session, executor, cwd, generation) = if let Some(request) = &request {
            let binding = binding
                .as_ref()
                .context("confirm the remote Task binding before operating")?;
            store.verify_historical_binding(binding)?;
            crate::store::validate_operation_binding_from(&store.connection, request)?;
            ensure!(
                request.network_id == project.space.as_str()
                    && request.conversation_id == session.conversation_id
                    && request.operation_id == field(body, "authorization_id")?
                    && request.target_node == target
                    && request.binding.as_ref() == Some(binding)
                    && request.session_id == binding.binding.session_id
                    && binding.binding.task_ref == creation.hash,
                "SDK policy refresh changed its current execution binding"
            );
            let executor = field(&binding.binding.open_spec, "executor")?.to_owned();
            let cwd = field(&binding.binding.open_spec, "cwd")?.to_owned();
            if action == Action::ExecutionOpen {
                ensure!(
                    request.action["executor"].as_str() == Some(executor.as_str())
                        && request.action["cwd"].as_str() == Some(cwd.as_str()),
                    "SDK open changed its confirmed executor or workspace"
                );
            }
            if let Some(original) = &origin.execution {
                ensure!(
                    request.target_node == original.target_node
                        && request.session_id == original.session_id
                        && request.binding == original.binding,
                    "SDK continuation changed its original executor or binding"
                );
            }
            (
                request.session_id.clone(),
                executor,
                cwd,
                binding.binding.generation,
            )
        } else {
            let executor = field(body, "executor")?.to_owned();
            let cwd = field(body, "cwd")?.to_owned();
            let execution_session = id(body, "session_id")?;
            ensure!(
                field(body, "target_node")? == target,
                "SDK binding changed its selected executor key"
            );
            if let Some(binding) = &binding {
                store.verify_historical_binding(binding)?;
                ensure!(
                    binding.binding.generation == 1
                        && binding.binding.task_ref == creation.hash
                        && binding.binding.target_node == target
                        && binding.binding.session_id == execution_session
                        && binding.binding.open_spec["executor"].as_str()
                            == Some(executor.as_str())
                        && binding.binding.open_spec["cwd"].as_str() == Some(cwd.as_str()),
                    "SDK binding retry changed its original choice"
                );
            }
            (execution_session, executor, cwd, 1)
        };
        crate::identity::execution_dispatch::validate_remote_resource_path(&cwd)?;
        let context = InvocationContext {
            conversation: Some(session.conversation_id.clone()),
            task: Some(task),
            execution_session: Some(SessionRef {
                target: DeviceId::from_key(&selection.target_public_key),
                session_id: execution_session,
            }),
            executor_kind: Some(executor),
            workspace_resource: Some(project.resource.clone()),
            sdk_session: Some(session.sdk_session.clone()),
        };
        let facts = RequestFacts {
            resource: project,
            action,
            audience,
            operation_id: id(body, "authorization_id")?,
            body_hash: ObjectHash::body(body)?,
            context: context.clone(),
        };
        let mut read_context = context;
        read_context.sdk_session = None;
        let context = PrepareContext {
            context: read_context,
            binding_generation: Some(generation),
            canonical_cwd: Some(cwd),
            execution_target_public_key: Some(selection.target_public_key.clone()),
        };
        Ok(SdkTaskPolicyInput {
            parent: origin.parent,
            facts,
            selection,
            context,
            binding,
        })
    }

    fn cached_sdk_task_policy(
        &self,
        store: &Store,
        input: &SdkTaskPolicyInput,
        now: u64,
    ) -> Result<Option<VerifiedTaskCurrentPolicy>> {
        let policies = task_policy_candidates_in(
            &store.connection,
            &store.policy,
            &PublicKey::parse(&self.identity.public_key_hex()?)?,
            &input.facts.resource,
            input
                .facts
                .context
                .task
                .as_ref()
                .context("SDK policy has no Task")?,
            input
                .facts
                .context
                .conversation
                .as_deref()
                .context("SDK policy has no conversation")?,
            input
                .facts
                .context
                .executor_kind
                .as_deref()
                .context("SDK policy has no executor")?,
            input.facts.action,
            input.binding.as_ref(),
            now,
        );
        let policies = match policies {
            Ok(policies) => policies,
            Err(error) if policy_sync_needed(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(policies.into_iter().find(|policy| {
            policy
                .check_scope(&input.facts, &input.selection, &input.context, now)
                .is_ok()
        }))
    }

    pub(super) async fn ensure_sdk_execution_task_policy(
        &self,
        session: &AgentSession,
        body: &Value,
    ) -> Result<Option<VerifiedTaskCurrentPolicy>> {
        if !self.is_execution_dispatch(body)? {
            return Ok(None);
        }
        let (input, pending) = {
            let mut store = self.store.lock().await;
            SignatureVerification::new().run(|| -> Result<_> {
                let now = now_ms() as u64;
                let input = self
                    .sdk_task_policy_input(&store, session, body, now)
                    .with_context(|| {
                        format!(
                            "SDK Task policy input; actual={:?}",
                            SignatureVerification::current_counts()
                        )
                    })?;
                if store
                    .execution_dispatch_facts(body, &input.facts.audience, now)
                    .is_ok()
                {
                    return Ok((input, SdkTaskPolicyRead::Primary));
                }
                if let Some(current) = self
                    .cached_sdk_task_policy(&store, &input, now)
                    .with_context(|| {
                        format!(
                            "SDK Task policy cached selection; actual={:?}",
                            SignatureVerification::current_counts()
                        )
                    })?
                {
                    return Ok((input, SdkTaskPolicyRead::Cached(current)));
                }
                let pending = store
                    .prepare_node_task_policy_read(
                        &self.identity,
                        &input.parent,
                        &session.service,
                        &session.grant_leaf,
                        &input.facts,
                        &input.selection,
                        &input.context,
                        RemoteExecutionPolicyPurpose::Execution,
                        now,
                        now.checked_add(INVOCATION_MAX_AGE_MS)
                            .context("SDK policy read deadline overflow")?
                            .min(session.expires_at_ms),
                    )
                    .context("SDK Task policy signed Node read preparation")?;
                Ok((input, SdkTaskPolicyRead::Pending(pending)))
            })?
        };
        let pending = match pending {
            SdkTaskPolicyRead::Primary => return Ok(None),
            SdkTaskPolicyRead::Cached(current) => return Ok(Some(current)),
            SdkTaskPolicyRead::Pending(pending) => pending,
        };
        let mut client = crate::client::NodePolicyReadClient::connect(
            &self.network,
            &self.identity,
            pending.trust(),
        )
        .await
        .context("SDK Node policy Noise connection")?;
        let NodeReadResponse::Snapshot(snapshot) = client
            .read(&NodeReadRequest::Prepare(pending.request().clone()))
            .await
            .context("SDK Node policy remote prepare")?
        else {
            bail!("SDK policy prepare returned another read stage");
        };
        let reference = {
            let mut store = self.store.lock().await;
            SignatureVerification::new().run(|| {
                store
                    .begin_node_task_policy_read(
                        pending,
                        &snapshot,
                        client.actual_noise_peer(),
                        now_ms() as u64,
                    )
                    .with_context(|| {
                        format!(
                            "SDK Node policy import begin; actual={:?}",
                            SignatureVerification::current_counts()
                        )
                    })
            })?
        };
        let mut cursor = None;
        loop {
            let request = RemoteExecutionPolicyPageRequest {
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                request_hash: snapshot.payload.request_hash.clone(),
                cursor: cursor.clone(),
            };
            let NodeReadResponse::Page(page) = client
                .read(&NodeReadRequest::Page(request))
                .await
                .context("SDK Node policy remote page")?
            else {
                bail!("SDK policy page returned another read stage");
            };
            {
                let mut store = self.store.lock().await;
                SignatureVerification::new().run(|| {
                    store
                        .append_node_task_policy_page(&reference, &page, now_ms() as u64)
                        .with_context(|| {
                            format!(
                                "SDK Node policy import page {}; actual={:?}",
                                page.payload.page_index,
                                SignatureVerification::current_counts()
                            )
                        })
                })?;
            }
            if page.payload.complete {
                break;
            }
            cursor = Some(
                page.payload
                    .next_cursor
                    .context("SDK policy page has no next cursor")?,
            );
        }
        let NodeReadResponse::Witness(witness) = client
            .read(&NodeReadRequest::Finish(RemoteExecutionPolicyPageRequest {
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                request_hash: snapshot.payload.request_hash.clone(),
                cursor: None,
            }))
            .await
            .context("SDK Node policy remote finish")?
        else {
            bail!("SDK policy finish returned another read stage");
        };
        let mut store = self.store.lock().await;
        SignatureVerification::new().run(|| {
            store
                .finish_node_task_policy_read(&reference, &witness, now_ms() as u64)
                .with_context(|| {
                    format!(
                        "SDK Node policy import finish; actual={:?}",
                        SignatureVerification::current_counts()
                    )
                })?;
            self.cached_sdk_task_policy(&store, &input, now_ms() as u64)
                .with_context(|| {
                    format!(
                        "SDK Node policy completed selection; actual={:?}",
                        SignatureVerification::current_counts()
                    )
                })?
                .map(Some)
                .context(
                    "NeedPolicySync: completed SDK policy no longer matches its current source",
                )
        })
    }
}
