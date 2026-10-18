use super::{
    admission::{check_policy_heads_in, consume_execution_request_dispatch_in, validate_basis_in},
    journal::PolicyState,
    policy::{FreshnessProof, RequestTransport, request_execution_dispatch_facts_from},
    task_execution_context::TaskScopedAuthorization,
};
use crate::{
    binding::{SignedBinding, TaskBinding},
    crypto::LocalIdentity,
    engine::EngineAction,
    model::{MAX_EVENT_BYTES, OperationRecord, OperationRequest, OperationState},
    store::{Store, validate_operation_binding_from},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    Action, Audience, ObjectHash, PolicyDomain, PolicyStamp, PublicKey, RequestFacts, ResourceId,
    ResourceRef, SpaceId, TransportEvidence, VerifiedAuthorization, VerifiedPolicySet,
    node_execution_policy::VerifiedTaskCurrentPolicy,
    prepare::VerifiedExecutionDestination,
    remote_execution_policy::{RemoteExecutionPolicyPurpose, RemoteExecutionPolicySelection},
};

#[derive(Clone, Debug)]
pub struct VerifiedDispatchFacts {
    facts: RequestFacts,
    destination: VerifiedExecutionDestination,
}

impl VerifiedDispatchFacts {
    pub fn facts(&self) -> &RequestFacts {
        &self.facts
    }
    pub fn destination(&self) -> &VerifiedExecutionDestination {
        &self.destination
    }

    pub(crate) fn revalidate(
        &self,
        connection: &Connection,
        policy: &PolicyState,
        scoped: &TaskScopedAuthorization,
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<RequestFacts> {
        let current = dispatch_facts_from(
            connection, scoped.state(policy.verified()), body, audience, now_ms,
        )?;
        ensure!(
            current.facts == self.facts
                && current.destination.grant_leaf() == self.destination.grant_leaf()
                && current.destination.heads() == self.destination.heads(),
            "execution destination changed before admission"
        );
        scoped.check_current_in(connection, policy.verified(), &current.facts, now_ms)?;
        if scoped.task_policy().is_none() {
            check_destination_heads_in(connection, current.destination.heads())?;
        }
        Ok(current.facts)
    }
}

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    body[field]
        .as_str()
        .with_context(|| format!("{field} is required"))
}

pub(crate) fn validate_remote_resource_path(value: &str) -> Result<&str> {
    use std::path::{Component, Path, PathBuf};
    let path = Path::new(value);
    ensure!(
        value.len() <= 4096
            && !value.contains('\0')
            && path.is_absolute()
            && path.components().all(|component| {
                matches!(component, Component::RootDir | Component::Normal(_))
            })
            && path.components().collect::<PathBuf>().to_str() == Some(value),
        "remote resource path must be absolute and lexically normalized"
    );
    Ok(value)
}

pub(super) fn check_destination_heads_in(
    connection: &Connection,
    heads: &[PolicyStamp],
) -> Result<()> {
    for stamp in heads {
        let (kind, id) = match &stamp.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual: Option<(u64,u64,String)> = connection.query_row(
            "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
            params![kind,id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        ensure!(
            actual
                == Some((
                    stamp.authority_epoch,
                    stamp.sequence,
                    stamp.head.to_string()
                )),
            "execution destination policy changed before commit"
        );
    }
    Ok(())
}

pub(super) fn dispatch_facts_from(
    connection: &Connection,
    policies: &VerifiedPolicySet,
    body: &Value,
    audience: &Audience,
    now_ms: u64,
) -> Result<VerifiedDispatchFacts> {
    ensure!(
        matches!(
            body["method"].as_str(),
            Some("operate" | "bind_execution" | "rebind_execution" | "resolve_unknown")
        ),
        "remote dispatch method is not supported"
    );
    let project = ResourceRef {
        space: SpaceId::parse(text(body, "space_id")?)?,
        resource: ResourceId::parse(text(body, "resource_id")?)?,
    };
    let resource: ResourceRef = serde_json::from_value(body["execution_resource"].clone())?;
    let key: PublicKey = serde_json::from_value(body["target_public_key"].clone())?;
    let grant: ObjectHash = serde_json::from_value(body["execution_grant"].clone())?;
    let mut context = policies
        .grant(&grant)
        .context("remote execution grant is unavailable")?
        .payload
        .constraints
        .context
        .clone();
    context.workspace_resource = Some(project.resource.clone());
    let destination = policies.verify_execution_destination(
        &project,
        audience,
        &resource,
        &key,
        &grant,
        &context,
        crate::model::authorization_action(body)?,
        now_ms,
    )?;
    let facts = request_execution_dispatch_facts_from(
        connection,
        policies,
        body,
        audience,
        &destination,
        now_ms,
    )?;
    Ok(VerifiedDispatchFacts { facts, destination })
}

impl Store {
    pub fn execution_share_request(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        body: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        self.execution_share_request_in_transport(identity, auth, None, body, now_ms)
    }

    pub(crate) fn execution_share_request_for_transport(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        transport: &RequestTransport,
        body: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        self.execution_share_request_in_transport(identity, auth, Some(transport), body, now_ms)
    }

    fn execution_share_request_in_transport(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        transport: Option<&RequestTransport>,
        body: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        use vibeface_secure::authorization::{
            AUTH_VERSION, Actor, DeviceId, EventRef, INVOCATION_MAX_AGE_MS, MAX_PROOF_BYTES, Nonce,
            ServiceId, Signed,
            remote_share::{RemoteShareIntent, RemoteSharePurpose, RemoteShareTaskAttestation},
        };
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            auth.action() == Action::ConversationRead
                && matches!(auth.actor(), Actor::Account(_))
                && auth
                    .signed_invocation()
                    .payload
                    .context
                    .sdk_session
                    .is_none()
                && body["method"] == "execution_share_request"
                && auth.body_hash() == &ObjectHash::body(body)?,
            "execution sharing requires a current Account task read"
        );
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        if let Some(transport) = transport {
            let facts = RequestFacts {
                resource: auth.resource().clone(), action: auth.action(), audience: auth.audience().clone(),
                operation_id: auth.operation_id().into(), body_hash: ObjectHash::body(body)?,
                context: auth.signed_invocation().payload.context.clone(),
            };
            super::policy::authorize_request_invocation_for_body(
                current.verified(), auth.signed_invocation(), &facts, transport, body, now_ms,
            )?;
        }
        current.verified().check_current(auth, now_ms)?;
        check_policy_heads_in(&tx, current.verified(), auth, now_ms)?;
        let encoded: String = tx
            .query_row(
                "SELECT envelope FROM events WHERE space_id=? AND hash=? AND status='ready'",
                params![auth.resource().space.as_str(), text(body, "task_ref")?],
                |row| row.get(0),
            )
            .context("source sharing requires the exact ready Task")?;
        let creation = serde_json::from_str(&encoded)?;
        let event = self.verify_event(auth.resource().space.as_str(), &creation)?;
        ensure!(
            creation.hash == text(body, "task_ref")?
                && event.resource_id == auth.resource().resource
                && event.scope_id == text(body, "conversation_id")?
                && matches!(&event.body, crate::model::EventBody::TaskCreated { task_id, .. }
                    if task_id == text(body, "task_id")?)
                && auth
                    .signed_invocation()
                    .payload
                    .context
                    .conversation
                    .as_deref()
                    == Some(event.scope_id.as_str()),
            "execution sharing task history differs from the selected Project"
        );
        let Actor::Account(account) = auth.actor() else {
            unreachable!()
        };
        let options = &body["share_request"];
        let purpose: RemoteSharePurpose = serde_json::from_value(options["purpose"].clone())?;
        let session = text(options, "session_id")?;
        let executor = text(options, "executor_kind")?;
        serde_json::from_value::<crate::engine::ExecutorKind>(Value::String(executor.into()))?;
        let expires = options["expires_at_ms"]
            .as_u64()
            .context("share request expiry is required")?;
        ensure!(
            expires > now_ms && expires - now_ms <= INVOCATION_MAX_AGE_MS,
            "source sharing requires a bounded future deadline"
        );
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let source = PublicKey::of(&signer);
        let record = current
            .verified()
            .resource(&auth.resource().space, &auth.resource().resource)
            .context("source Project is missing")?;
        current.verified().verify_service_device(
            &auth.resource().space,
            &DeviceId::from_key(&source),
            now_ms,
        )?;
        ensure!(
            record.payload.kind == vibeface_secure::authorization::ResourceKind::Project
                && record.payload.host == ServiceId::from_key(&source)
                && auth.audience().service == ServiceId::from_key(&source)
                && auth.audience().device == DeviceId::from_key(&source),
            "source sharing targets another Project host"
        );
        let optional = |name: &str| -> Result<Option<Value>> {
            Ok(options.get(name).filter(|value| !value.is_null()).cloned())
        };
        let old_binding: Option<SignedBinding> = optional("old_binding")?
            .map(serde_json::from_value)
            .transpose()?;
        if purpose == RemoteSharePurpose::Execution {
            ensure!(
                old_binding.is_none() && options["old_execution_receipt"].is_null(),
                "ordinary execution sharing cannot carry an old binding"
            );
        } else {
            let old = old_binding
                .as_ref()
                .context("existing binding is required")?;
            ensure!(
                self.task_binding(auth.resource().space.as_str(), &creation.hash)?
                    .as_ref()
                    == Some(old)
                    && old.binding.conversation_id == event.scope_id
                    && old.binding.task_id == text(body, "task_id")?
                    && old.binding.session_id == session,
                "source sharing changed the current binding or exact Task"
            );
        }
        let attestation = Signed::sign(
            RemoteShareTaskAttestation {
                version: AUTH_VERSION,
                project: auth.resource().clone(),
                task: EventRef::parse(&creation.hash)?,
                task_id: text(body, "task_id")?.into(),
                conversation_id: event.scope_id,
                author: event.author,
                sequence: event.sequence,
                source_node_public_key: source.clone(),
                issued_at_ms: now_ms,
                expires_at_ms: expires,
            },
            &signer,
        )?;
        let intent = RemoteShareIntent {
            version: AUTH_VERSION,
            purpose,
            project: auth.resource().clone(),
            task_creation: attestation,
            source_node_public_key: source,
            requester: account.clone(),
            requester_device: auth.signed_invocation().payload.device.clone(),
            saved_pins: Vec::new(),
            policy_heads: auth.dependencies().to_vec(),
            session_id: session.into(),
            executor_kind: executor.into(),
            execution_resource: optional("execution_resource")?
                .map(serde_json::from_value)
                .transpose()?,
            target_public_key: optional("target_public_key")?
                .map(serde_json::from_value)
                .transpose()?,
            old_binding,
            old_execution_receipt: optional("old_execution_receipt")?
                .map(serde_json::from_value)
                .transpose()?,
            nonce: Nonce::random()?,
            issued_at_ms: now_ms,
            expires_at_ms: expires,
        };
        ensure!(
            serde_json::to_vec(&intent)?.len() <= MAX_PROOF_BYTES,
            "source sharing input exceeds byte limit"
        );
        tx.commit()?;
        Ok(serde_json::to_value(intent)?)
    }

    pub fn execution_dispatch_targets(
        &self,
        session: &crate::model::AgentSession,
        task_id: &str,
        executor: &str,
        action: Action,
        now_ms: u64,
    ) -> Result<Vec<Value>> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        use vibeface_secure::authorization::{Actor, EventRef};
        ensure!(
            matches!(
                action,
                Action::ExecutionOpen
                    | Action::ExecutionSend
                    | Action::ExecutionSteer
                    | Action::ExecutionInterrupt
                    | Action::ExecutionRespondApproval
            ),
            "target discovery requires a concrete execution action"
        );
        let stored = self
            .local_value(
                session.space_id.as_str(),
                &format!("agent-session:{}", session.sdk_session.session_id),
            )?
            .context("target discovery has no persisted SDK session")?;
        ensure!(
            stored == serde_json::to_value(session)?
                && now_ms < session.expires_at_ms
                && session.sdk_session.target == session.device,
            "target discovery changed its current SDK delegation"
        );
        let origin = self.agent_origin(session, now_ms)?;
        let creation =
            self.task_creation(session.space_id.as_str(), &session.conversation_id, task_id)?;
        let event = self.verify_event(session.space_id.as_str(), &creation)?;
        ensure!(
            event.resource_id == session.resource_id,
            "target discovery refers to another Project task"
        );
        let task_ref = EventRef::parse(&creation.hash)?;
        let binding = self.task_binding(session.space_id.as_str(), &creation.hash)?;
        if let Some((current_task, current_ref)) = &origin.task {
            ensure!(
                current_task == task_id && current_ref == &task_ref,
                "continuation target discovery escaped its own task"
            );
        } else {
            let crate::model::EventCause::Invocation { admission } = &event.cause else {
                anyhow::bail!("SDK task has no admitted creation");
            };
            let own_sdk = admission.invocation.payload.actor == Actor::Service(session.service.clone())
                    && admission.invocation.payload.context.sdk_session.as_ref()
                        == Some(&session.sdk_session)
                    && admission.source_body["parent_invocation"].as_str()
                        == Some(session.parent_invocation.as_str());
            let account_bound = matches!(admission.invocation.payload.actor,Actor::Account(_))
                && admission.invocation.payload.actor == session.initiator
                && binding.is_some();
            ensure!(own_sdk || account_bound,
                "initial SDK discovery requires its own task or its initiator's confirmed binding");
        }
        if let Some(binding) = &binding {
            self.verify_historical_binding(binding)?;
            ensure!(
                binding.binding.open_spec["executor"] == executor,
                "target discovery changed the confirmed executor"
            );
        }
        let project = ResourceRef {
            space: session.space_id.clone(),
            resource: session.resource_id.clone(),
        };
        let audience = Audience {
            service: session.service.clone(),
            device: session.device.clone(),
        };
        let primary = self.policy.verified();
        let source = primary.verify_service_device(&project.space, &audience.device, now_ms)?;
        let record = primary.resource(&project.space, &project.resource)
            .context("target discovery has no Project resource")?;
        ensure!(
            source.payload.subject == Actor::Service(session.service.clone())
                && record.payload.kind == vibeface_secure::authorization::ResourceKind::Project
                && record.payload.host == session.service,
            "target discovery changed the current Project host"
        );
        let candidates = super::foreign_execution_policy::task_policy_candidates_in(
            &self.connection, &self.policy, &source.payload.public_key, &project, &task_ref,
            &session.conversation_id, executor, action, binding.as_ref(), now_ms,
        )?;
        let mut targets = Vec::new();
        for candidate in candidates {
            let selection = candidate.selection();
            let context = &candidate.context().context;
            let selected_session = context.execution_session.as_ref()
                .context("approved execution policy has no selected session")?;
            ensure!(
                context.task.as_ref() == Some(&task_ref)
                    && context.conversation.as_deref() == Some(session.conversation_id.as_str())
                    && context.executor_kind.as_deref() == Some(executor)
                    && context.workspace_resource.as_ref() == Some(&project.resource),
                "approved execution policy escaped the current SDK task"
            );
            let destination = candidate.state().verify_execution_destination(
                &project, &audience, &selection.execution_resource, &selection.target_public_key,
                &selection.execution_grant, context, action, now_ms,
            )?;
            super::foreign_execution_policy::check_task_policy_heads_in(&self.connection, &candidate)?;
            let target = crate::crypto::fingerprint(destination.public_key().as_str())?;
            ensure!(
                binding.as_ref().is_none_or(|binding| {
                    binding.binding.target_node == target
                        && binding.binding.session_id == selected_session.session_id
                }),
                "approved execution policy changed the confirmed SDK binding"
            );
            let cwd = binding.as_ref().and_then(|binding|binding.binding.open_spec["cwd"].as_str())
                .or(candidate.context().canonical_cwd.as_deref());
            targets.push(json!({"execution_resource":selection.execution_resource,
                "target_public_key":destination.public_key(),"execution_grant":selection.execution_grant,
                "target_node":target,"session_id":selected_session.session_id,
                "executor":executor,"task_id":task_id,"task_ref":task_ref,"cwd":cwd}));
        }
        targets.sort_by_key(|value| value.to_string());
        targets.dedup();
        Ok(targets)
    }

    pub fn execution_dispatch_metadata_facts(
        &self,
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<Option<RequestFacts>> {
        use vibeface_secure::authorization::{Signed, execution_handoff::ProjectExecutionSource};
        if !matches!(
            body["method"].as_str(),
            Some("operation" | "session" | "reconcile")
        ) {
            return Ok(None);
        }
        let Some(operation) = body["operation_id"].as_str() else {
            ensure!(
                body.get("execution_source").is_none(),
                "metadata source needs an original operation ID"
            );
            return Ok(None);
        };
        let space = text(body, "space_id")?;
        let Some(wire) = self.execution_dispatch_handoff(space, operation)? else {
            ensure!(
                body.get("execution_source").is_none(),
                "metadata operation has no durable remote handoff"
            );
            return Ok(None);
        };
        let stored = self
            .operation(space, operation)?
            .context("remote metadata operation is missing")?;
        let original = self.operation_admission_proof(space, operation)?;
        ensure!(
            original.source_body["method"] == "operate"
                && serde_json::from_value::<OperationRequest>(
                    original.source_body["request"].clone()
                )? == stored.request,
            "remote metadata differs from its original accepted operation"
        );
        let project = &original.invocation.payload.resource;
        ensure!(
            project.space.as_str() == space
                && project.resource.as_str() == text(body, "resource_id")?
                && original.invocation.payload.audience == *audience,
            "metadata request targets another Project"
        );
        let source: Signed<ProjectExecutionSource> =
            serde_json::from_value(body["execution_source"].clone())?;
        let binding: SignedBinding = serde_json::from_value(body["binding"].clone())?;
        ensure!(
            wire.handoff.source_attestation.as_ref() == Some(&source)
                && stored.request.binding.as_ref() == Some(&binding)
                && source.payload.operation_id == operation,
            "metadata evidence differs from the durable handoff"
        );
        let mut context = self
            .policy
            .verified()
            .verify_execution_dispatch_metadata(project, audience, &source, &binding, now_ms)?;
        for (field, expected) in [
            ("conversation_id", stored.request.conversation_id.as_str()),
            ("target_node", stored.request.target_node.as_str()),
            ("session_id", stored.request.session_id.as_str()),
            ("task_ref", binding.binding.task_ref.as_str()),
        ] {
            if let Some(actual) = body[field].as_str() {
                ensure!(
                    actual == expected,
                    "metadata request changed the admitted operation context"
                );
            }
        }
        if let Some(task) = body.get("task_id") {
            ensure!(
                *task == serde_json::to_value(&stored.request.task_id)?,
                "metadata request changed its task"
            );
        }
        if body["method"] == "session" {
            ensure!(
                text(body, "target_node")? == stored.request.target_node
                    && text(body, "session_id")? == stored.request.session_id,
                "session metadata requires its actual admitted target and session"
            );
        }
        super::admission::validate_persisted_admission(&self.connection, &original)?;
        if !body["sdk_session"].is_null() {
            use vibeface_secure::authorization::{Actor, SessionRef};
            let session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
            let encoded: String = self.connection.query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, format!("agent-session:{}", session.session_id)],
                |row| row.get(0),
            )?;
            let saved: crate::model::AgentSession = serde_json::from_str(&encoded)?;
            ensure!(
                saved.space_id == project.space
                    && saved.resource_id == project.resource
                    && saved.sdk_session == session
                    && saved.sdk_session.target == saved.device
                    && saved.device == audience.device
                    && saved.service == audience.service
                    && saved.conversation_id == stored.request.conversation_id
                    && body["parent_invocation"].as_str() == Some(saved.parent_invocation.as_str())
                    && now_ms < saved.expires_at_ms
                    && original.invocation.payload.actor == Actor::Service(saved.service.clone())
                    && original.invocation.payload.device == saved.device
                    && original.invocation.payload.initiator == saved.initiator,
                "remote metadata differs from the current SDK delegation"
            );
            let origin = self.agent_origin(&saved, now_ms)?;
            if let Some((task_id, task_ref)) = &origin.task {
                let execution = origin
                    .execution
                    .as_ref()
                    .context("SDK task has no execution origin")?;
                ensure!(
                    stored.request.task_id.as_ref() == Some(task_id)
                        && binding.binding.task_ref == task_ref.as_str()
                        && stored.request.target_node == execution.target_node
                        && stored.request.session_id == execution.session_id
                        && stored.request.binding == execution.binding,
                    "SDK metadata escaped its current task or execution binding"
                );
            } else {
                ensure!(
                    original.invocation.payload.context.sdk_session.as_ref() == Some(&session)
                        && original.source_body["parent_invocation"].as_str()
                            == Some(saved.parent_invocation.as_str()),
                    "initial SDK metadata can inspect only its own accepted operation"
                );
            }
            context.context.sdk_session = Some(session);
            let facts = RequestFacts {
                resource: project.clone(),
                action: crate::model::authorization_action(body)?,
                audience: audience.clone(),
                operation_id: text(body, "authorization_id")?.into(),
                body_hash: ObjectHash::body(body)?,
                context: context.context,
            };
            let delegated = self.policy.verified().verify_service_delegation(
                &origin.parent,
                &saved.service,
                &saved.grant_leaf,
                &facts,
                now_ms,
            )?;
            ensure!(
                delegated.device() == &saved.device
                    && delegated.initiator() == &saved.initiator
                    && delegated.initiating_grant() == saved.initiating_grant.as_ref(),
                "SDK metadata has no current parent delegation"
            );
            return Ok(Some(facts));
        }
        ensure!(
            body["parent_invocation"].is_null(),
            "Account metadata cannot claim an SDK parent"
        );
        Ok(Some(RequestFacts {
            resource: project.clone(),
            action: crate::model::authorization_action(body)?,
            audience: audience.clone(),
            operation_id: text(body, "authorization_id")?.into(),
            body_hash: ObjectHash::body(body)?,
            context: context.context,
        }))
    }

    pub fn execution_dispatch_facts(
        &self,
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<VerifiedDispatchFacts> {
        dispatch_facts_from(
            &self.connection,
            self.policy.verified(),
            body,
            audience,
            now_ms,
        )
    }

    pub(crate) fn execution_dispatch_facts_with_task_policy(
        &self,
        task: &VerifiedTaskCurrentPolicy,
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<VerifiedDispatchFacts> {
        super::foreign_execution_policy::check_task_policy_heads_in(&self.connection, task)?;
        let selection = RemoteExecutionPolicySelection {
            execution_resource: serde_json::from_value(body["execution_resource"].clone())?,
            target_public_key: serde_json::from_value(body["target_public_key"].clone())?,
            execution_grant: serde_json::from_value(body["execution_grant"].clone())?,
        };
        ensure!(
            selection == *task.selection()
                && task.request().expected_action() == crate::model::authorization_action(body)?
                && matches!(
                    task.request().purpose(),
                    RemoteExecutionPolicyPurpose::Execution
                        | RemoteExecutionPolicyPurpose::Resolver
                ),
            "dispatch differs from the current Task policy selection or purpose"
        );
        let dispatch = dispatch_facts_from(&self.connection, task.state(), body, audience, now_ms)?;
        let mut context = dispatch.facts().context.clone();
        context.sdk_session = None;
        ensure!(
            context == task.context().context,
            "dispatch differs from the current Task policy execution context"
        );
        task.check_scope(dispatch.facts(), &selection, task.context(), now_ms)?;
        Ok(dispatch)
    }

    pub(crate) fn execution_dispatch_facts_in_scope(
        &self,
        scoped: &TaskScopedAuthorization,
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<VerifiedDispatchFacts> {
        let dispatch = dispatch_facts_from(
            &self.connection, scoped.state(self.policy.verified()), body, audience, now_ms,
        )?;
        scoped.check_current_in(&self.connection, self.policy.verified(), dispatch.facts(), now_ms)?;
        Ok(dispatch)
    }

    pub(crate) fn bind_execution_dispatch(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        scoped: &TaskScopedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<SignedBinding> {
        self.bind_execution_request_dispatch(
            identity, dispatch, scoped, proof, &RequestTransport::Existing(transport.clone()), body, now_ms,
        )
    }

    pub(crate) fn bind_execution_request_dispatch(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        scoped: &TaskScopedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        body: &Value,
        now_ms: u64,
    ) -> Result<SignedBinding> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let auth = scoped.authorization();
        transport.check_current(auth, now_ms)?;
        ensure!(
            auth.action() == Action::ExecutionOpen && body["method"] == "bind_execution",
            "remote binding requires execution-open authorization"
        );
        let space = auth.resource().space.as_str();
        let creation = self.task_creation(
            space,
            text(body, "conversation_id")?,
            text(body, "task_id")?,
        )?;
        let event = self.verify_event(space, &creation)?;
        ensure!(
            creation.hash == text(body, "task_ref")?
                && event.resource_id == auth.resource().resource
                && event.author == identity.node_id()?,
            "remote binding task authority differs"
        );
        let binding = TaskBinding {
            network_id: space.into(),
            conversation_id: event.scope_id,
            task_ref: creation.hash,
            task_id: text(body, "task_id")?.into(),
            authority_node: event.author,
            generation: 1,
            target_node: text(body, "target_node")?.into(),
            session_id: text(body, "session_id")?.into(),
            open_spec: json!({"type":"open","executor":text(body,"executor")?,"cwd":text(body,"cwd")?}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        self.verify_binding_task(&binding, false)?;
        let signed = crate::binding::sign_binding(identity, &binding)?;
        let hash = crate::binding::binding_hash(&binding)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let facts = dispatch.revalidate(&tx, &current, scoped, body, auth.audience(), now_ms)?;
        super::policy::authorize_request_invocation_for_body(
            scoped.state(current.verified()), auth.signed_invocation(), &facts, transport, body, now_ms,
        )?;
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let current_binding: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, current_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(current_binding) = current_binding {
            ensure!(
                serde_json::from_str::<String>(&current_binding)? == hash,
                "task is already bound; changing it requires execution-rebind authorization"
            );
            let (invocation, source): (String,String) = tx.query_row(
                "SELECT signed_invocation,source_body FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
                params![space,auth.operation_id()], |row| Ok((row.get(0)?,row.get(1)?)))?;
            ensure!(
                serde_json::from_str::<Value>(&invocation)?
                    == serde_json::to_value(auth.signed_invocation())?
                    && serde_json::from_str::<Value>(&source)? == *body,
                "binding retry differs from its original authorization"
            );
            scoped.state(current.verified()).verify_freshness(
                auth,
                &proof.challenge,
                &proof.receipts,
                now_ms,
            )?;
            tx.commit()?;
            self.policy = current;
            return Ok(signed);
        }
        let admitted = consume_execution_request_dispatch_in(
            &tx,
            &current,
            scoped,
            proof,
            transport,
            body,
            None,
            now_ms,
            dispatch,
        )?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                space,
                format!("task-binding-record:{hash}"),
                serde_json::to_string(&signed)?
            ],
        )?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![space, current_key, serde_json::to_string(&hash)?],
        )?;
        scoped.bind_in(&tx, body, Some(&signed), admitted.admitted_at_ms)?;
        tx.commit()?;
        self.policy = current;
        Ok(signed)
    }
}

fn check_remote_session(
    body: &Value,
    request: &OperationRequest,
    dispatch: &VerifiedDispatchFacts,
    project_key: &PublicKey,
    now_ms: u64,
) -> Result<()> {
    use vibeface_secure::authorization::{
        Signed,
        execution_handoff::{ExecutionHandoffReceipt, verify_execution_session_snapshot},
    };
    let binding = request
        .binding
        .as_ref()
        .context("execution binding is missing")?;
    if request.action["type"] == "open" {
        ensure!(
            request.expected_revision == 0 && body.get("execution_session_receipt").is_none(),
            "new remote session cannot claim an existing session revision"
        );
        return Ok(());
    }
    let receipt: Signed<ExecutionHandoffReceipt> =
        serde_json::from_value(body["execution_session_receipt"].clone())?;
    let session = verify_execution_session_snapshot(
        &receipt,
        &dispatch.facts.resource,
        dispatch.destination.resource(),
        dispatch.destination.public_key(),
        binding,
        project_key,
        now_ms,
    )?;
    ensure!(
        session.revision == request.expected_revision
            && !matches!(session.state.as_str(), "unknown" | "closed" | "opening"),
        "remote session revision differs or its execution state is unavailable"
    );
    Ok(())
}

impl Store {
    pub(crate) fn accept_execution_dispatch(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        scoped: &TaskScopedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        request: &OperationRequest,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        self.accept_execution_request_dispatch(
            identity, dispatch, scoped, proof, &RequestTransport::Existing(transport.clone()), body, request, now_ms,
        )
    }

    pub(crate) fn accept_execution_request_dispatch(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        scoped: &TaskScopedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        body: &Value,
        request: &OperationRequest,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let auth = scoped.authorization();
        transport.check_current(auth, now_ms)?;
        ensure!(
            body["method"] == "operate"
                && body["request"] == serde_json::to_value(request)?
                && auth.resource() == &dispatch.facts.resource
                && auth.body_hash() == &ObjectHash::body(body)?
                && request.network_id == auth.resource().space.as_str()
                && vibeface_secure::authorization::DeviceId::from_key(&PublicKey::parse(
                    &identity.public_key_hex()?
                )?) == auth.audience().device,
            "remote operation differs from its Project authorization"
        );
        ensure!(
            !request.operation_id.is_empty()
                && !request.session_id.is_empty()
                && request.expected_revision < i64::MAX as u64
                && request.resources.len() <= 128
                && serde_json::to_vec(request)?.len() <= MAX_EVENT_BYTES,
            "invalid remote operation shape"
        );
        let binding = request
            .binding
            .as_ref()
            .context("remote operation binding is missing")?;
        self.verify_historical_binding(binding)?;
        let action = text(&request.action, "type")?;
        if action == "open" {
            let fields = request
                .action
                .as_object()
                .context("invalid open operation")?;
            ensure!(
                fields.len() == 3
                    && fields
                        .keys()
                        .all(|key| matches!(key.as_str(), "type" | "executor" | "cwd")),
                "unexpected open action field"
            );
            serde_json::from_value::<crate::engine::ExecutorKind>(
                request.action["executor"].clone(),
            )?;
        } else {
            serde_json::from_value::<EngineAction>(request.action.clone())?.validate()?;
        }
        let control = !matches!(action, "open" | "send");
        let resources = request
            .resources
            .iter()
            .map(|resource| validate_remote_resource_path(resource).map(str::to_owned))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            resources == request.resources
                && resources.windows(2).all(|pair| pair[0] < pair[1])
                && (!control || resources.is_empty()),
            "remote resources are not canonical or allowed"
        );
        if !control {
            ensure!(
                resources
                    .iter()
                    .any(|resource| Some(resource.as_str())
                        == binding.binding.open_spec["cwd"].as_str()),
                "remote working directory is not reserved"
            );
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let facts = dispatch.revalidate(&tx, &current, scoped, body, auth.audience(), now_ms)?;
        super::policy::authorize_request_invocation_for_body(scoped.state(current.verified()),
            auth.signed_invocation(),
            &facts,
            transport,
            body,
            now_ms,
        )?;
        scoped.state(current.verified())
            .verify_freshness(auth, &proof.challenge, &proof.receipts, now_ms)?;
        let previous: Option<(String,String,String,String,String)> = tx.query_row(
            "SELECT o.request,o.state,o.result,a.signed_invocation,a.source_body FROM operations o JOIN accepted_authorizations a ON a.space_id=o.space_id AND a.operation_id=o.operation_id WHERE o.space_id=? AND o.operation_id=?",
            params![request.network_id,request.operation_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        ).optional()?;
        if let Some((saved, state, result, invocation, source)) = previous {
            ensure!(
                serde_json::from_str::<OperationRequest>(&saved)? == *request
                    && serde_json::from_str::<Value>(&invocation)?
                        == serde_json::to_value(auth.signed_invocation())?
                    && serde_json::from_str::<Value>(&source)? == *body,
                "operation ID reused with a different request"
            );
            tx.commit()?;
            self.policy = current;
            return Ok(OperationRecord {
                request: request.clone(),
                state: serde_json::from_str(&state)?,
                result: serde_json::from_str(&result)?,
            });
        }
        validate_basis_in(&tx, request, &auth.resource().resource)?;
        check_remote_session(
            body,
            request,
            dispatch,
            &PublicKey::parse(&identity.public_key_hex()?)?,
            now_ms,
        )?;
        if action == "open" {
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE space_id=? AND target_node=? AND session_id=?)",
                params![request.network_id,request.target_node,request.session_id], |row| row.get(0))?;
            ensure!(
                !exists,
                "remote session has already been opened; reconcile its original operation"
            );
        }
        crate::store::check_execution_session_in(
            &tx,
            &request.network_id,
            &request.target_node,
            &request.session_id,
            control,
        )?;
        crate::store::check_execution_resources_in(&tx, &request.target_node, &resources)?;
        let admission = consume_execution_request_dispatch_in(
            &tx,
            &current,
            scoped,
            proof,
            transport,
            body,
            Some(&request.operation_id),
            now_ms,
            dispatch,
        )?;
        tx.execute("INSERT INTO operations(space_id,operation_id,target_node,session_id,request,state,control,result) VALUES(?,?,?,?,?,'\"accepted\"',?,'null')",
            params![request.network_id,request.operation_id,request.target_node,request.session_id,serde_json::to_string(request)?,control])?;
        for resource in &resources {
            tx.execute("INSERT INTO resource_reservations(space_id,operation_id,target_node,resource) VALUES(?,?,?,?)",
                params![request.network_id,request.operation_id,request.target_node,resource])?;
        }
        scoped.bind_in(&tx, body, None, admission.admitted_at_ms)?;
        super::event_authorization::append_admitted_dispatch_in(
            &tx, &current, identity, &admission, request, now_ms,
        )?;
        let checkpoint = super::admission::prepare_local_admission_checkpoint_in(
            &tx, &current, &admission,
        )?;
        tx.commit()?;
        self.policy = current;
        self.policy.local_admissions.insert(checkpoint);
        Ok(OperationRecord {
            request: request.clone(),
            state: OperationState::Accepted,
            result: Value::Null,
        })
    }
}

fn dispatch_authorization_from(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    space: &str,
    operation: &str,
    now_ms: u64,
) -> Result<(crate::model::SignedEvent, TaskScopedAuthorization)> {
    let source = super::event_authorization::retained_operation_dispatch_in(
        connection, policy, space, operation,
    )?
    .context("operation has no ready admitted dispatch")?;
    let event: crate::model::Event = serde_json::from_str(&source.canonical_body)?;
    let events = super::event_authorization::retained_event_dependencies_in(
        connection,
        space,
        &event.resource_id,
        &event.scope_id,
        std::slice::from_ref(&source.hash),
    )?;
    let retained = events
        .get(&source.hash)
        .context("operation has no ready admitted dispatch")?;
    let crate::model::EventCause::Invocation { admission } = &retained.event.cause else {
        anyhow::bail!("dispatch has no original admitted invocation");
    };
    let invocation = &admission.invocation.payload;
    let facts = RequestFacts {
        resource: invocation.resource.clone(),
        action: invocation.action,
        audience: invocation.audience.clone(),
        operation_id: invocation.operation_id.clone(),
        body_hash: ObjectHash::body(&admission.source_body)?,
        context: invocation.context.clone(),
    };
    let scoped = TaskScopedAuthorization::for_admitted_operation_in(
        connection, policy, identity, admission, &facts, now_ms,
    )?;
    let auth = super::event_authorization::event_authorization_in_view(
        policy,
        connection,
        scoped.state(policy.verified()),
        &retained.event,
        &events,
        now_ms,
    )?;
    ensure!(
        auth.invocation_hash() == scoped.authorization().invocation_hash()
            && auth.body_hash() == scoped.authorization().body_hash(),
        "dispatch scope differs from its current event authorization"
    );
    Ok((source, scoped))
}

struct RetainedDispatch {
    source: crate::model::SignedEvent,
    admission: crate::model::AdmissionProof,
    request: OperationRequest,
}

fn retained_dispatch_from(
    connection: &Connection,
    identity: &LocalIdentity,
    wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
) -> Result<RetainedDispatch> {
    use vibeface_secure::authorization::{DeviceId, execution_observation::ExecutionObservationTarget};
    let target = ExecutionObservationTarget::from_original(wire)?;
    let space = target.source.project.space.as_str();
    let operation = &target.source.operation_id;
    let encoded: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![space, format!("project-execution-handoff:{operation}")], |row| row.get(0),
    )?;
    ensure!(serde_json::from_str::<vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff>(&encoded)? == *wire,
        "remote result differs from its durable submitted carrier");
    let encoded: String = connection.query_row(
        "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
        params![space,operation],|row|row.get(0),
    )?;
    let request: OperationRequest = serde_json::from_str(&encoded)?;
    let admission = super::admission::admission_for_operation_from(connection,space,operation)?;
    let source = &wire.handoff.project_dispatch;
    let event: crate::model::Event = serde_json::from_str(&source.canonical_body)?;
    let (saved,resource,scope,author,sequence):(String,String,String,String,u64) = connection.query_row(
        "SELECT envelope,resource_id,scope_id,author,sequence FROM events WHERE space_id=? AND hash=? AND status='ready'",
        params![space,source.hash],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    )?;
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    let checkpoint = wire.handoff.source_attestation.as_ref()
        .context("remote result has no retained Project checkpoint")?;
    checkpoint.verify_signature()?;
    ensure!(checkpoint.issuer == local && checkpoint.payload == wire.handoff.source_statement()?
        && wire.invocation.issuer == local
        && admission.invocation.payload.audience.device == DeviceId::from_key(&local)
        && admission.invocation.payload.resource == target.source.project
        && serde_json::from_value::<OperationRequest>(admission.source_body["request"].clone())? == request
        && serde_json::from_str::<crate::model::SignedEvent>(&saved)? == *source
        && event.network_id == space && event.resource_id == target.source.project.resource
        && resource == event.resource_id.as_str() && scope == event.scope_id
        && author == event.author && sequence == event.sequence
        && event.scope_id == request.conversation_id
        && matches!(&event.cause,crate::model::EventCause::Invocation{admission:proof} if proof == &admission)
        && matches!(&event.body,crate::model::EventBody::DispatchRecorded{operation_id,action,..}
            if operation_id == operation && *action == serde_json::to_value(&request)?),
        "retained remote dispatch changed its host, admission, request or ready row");
    Ok(RetainedDispatch { source:source.clone(), admission, request })
}

pub(super) fn retained_execution_history_reference_in(
    connection: &Connection,
    identity: &LocalIdentity,
    reference: &vibeface_secure::authorization::remote_execution_history::RemoteExecutionHistoryReference,
) -> Result<vibeface_secure::authorization::execution_handoff::ExecutionProofUsage> {
    use vibeface_secure::authorization::{
        execution_handoff::{ExecutionProofUsage, SignedExecutionHandoff},
        remote_execution_history::{
            RemoteExecutionHistoryKind, RemoteExecutionHistoryReference,
            derive_remote_execution_history_reference, validate_remote_execution_history_reference,
        },
    };
    validate_remote_execution_history_reference(reference)?;
    let project = &reference.source.payload.project;
    let operation = &reference.source.payload.operation_id;
    let encoded: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![
            project.space.as_str(),
            format!("project-execution-handoff:{operation}")
        ],
        |row| row.get(0),
    )?;
    let original: SignedExecutionHandoff = serde_json::from_str(&encoded)?;
    let retained = retained_dispatch_from(connection, identity, &original)?;
    crate::crypto::verify_signed_event(
        project.space.as_str(),
        &retained.source,
        &identity.public_key_hex()?,
    )?;
    super::event_authorization::validate_retained_event_dependencies_in(
        connection,
        &retained.source,
    )?;
    ensure!(
        original.handoff.source_attestation.as_ref() == Some(&reference.source)
            && original.hash()? == reference.handoff_hash
            && original.handoff.target_public_key == *reference.target_public_key()
            && original.handoff.execution_resource == *reference.execution_resource()
            && retained.source.hash == reference.dispatch_ref.as_str()
            && ObjectHash::body(&retained.source)? == reference.dispatch_envelope_hash,
        "history changed its original durable Project handoff or ready dispatch"
    );
    let (actual, account, binding) = match reference.kind {
        RemoteExecutionHistoryKind::ExecutionResult => {
            ensure!(
                original.invocation == reference.old_submission,
                "execution history changed its original submitted invocation"
            );
            let (state, encoded): (String, String) = connection.query_row(
                "SELECT state,result FROM operations WHERE space_id=? AND operation_id=?",
                params![project.space.as_str(), operation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let value: Value = serde_json::from_str(&encoded)?;
            let matching = !value.is_null() && ObjectHash::body(&value)? == reference.actual_hash;
            let state: OperationState = serde_json::from_str(&state)?;
            ensure!(
                matching || !state.terminal(),
                "history has no matching immutable Project execution result"
            );
            (
                matching.then_some(value),
                retained.admission.clone(),
                retained.request.binding.clone(),
            )
        }
        RemoteExecutionHistoryKind::UnknownResolution
        | RemoteExecutionHistoryKind::TerminalReconciliation => {
            let (actual, wire) =
                super::execution_dispatch_resolution::retained_history_resolution_in(
                    connection, identity, reference,
                )?;
            ensure!(
                wire.resolution.observation.handoff == original,
                "resolution history changed its original durable execution handoff"
            );
            (actual, wire.resolution.account_admission, None)
        }
    };
    account.invocation.verify_signature()?;
    ensure!(
        account.invocation.hash()? == reference.account_invocation_hash
            && ObjectHash::body(&account.invocation)? == reference.expected_invocation_hash
            && ObjectHash::body(&account.source_body)? == reference.expected_body_hash
            && account.admitted_at_ms == reference.account_admitted_at_ms
            && binding.as_ref().map(ObjectHash::body).transpose()?
                == reference.expected_binding_hash,
        "history changed its original complete Account request or binding"
    );
    if let Some(actual) = actual {
        let derived: RemoteExecutionHistoryReference = serde_json::from_str(
            &derive_remote_execution_history_reference(
                &serde_json::to_string(&actual)?,
                &serde_json::to_string(&account.source_body)?,
                &serde_json::to_string(&account.invocation)?,
                &binding.as_ref().map(serde_json::to_string).transpose()?.unwrap_or_default(),
            )
            .map_err(anyhow::Error::msg)?,
        )?;
        ensure!(
            derived == *reference,
            "history changed its complete original Project artifact"
        );
    }
    let mut usage = ExecutionProofUsage::default();
    usage.record_retained_event(&retained.source)?;
    usage.record_signed(&reference.source)?;
    usage.record_signed(&reference.old_submission)?;
    usage.record_signed(&reference.observation_receipt)?;
    usage.record_signed(&account.invocation)?;
    if let Some(proof) = &reference.terminal_freshness {
        usage.record_metadata(&proof.challenge)?;
        for receipt in &proof.receipts {
            usage.record_signed(receipt)?;
        }
    }
    if let Some(binding) = &binding {
        crate::binding::verify_binding(binding, &identity.public_key_hex()?)?;
        usage.record_binding(binding)?;
    }
    usage.record_metadata(reference)?;
    usage.enforce_limits()?;
    Ok(usage)
}

fn retained_dispatch_authorization(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    retained: &RetainedDispatch,
    now_ms: u64,
) -> Result<(TaskScopedAuthorization, RequestFacts)> {
    let invocation = &retained.admission.invocation.payload;
    let facts = RequestFacts {
        resource:invocation.resource.clone(), action:invocation.action,
        audience:invocation.audience.clone(), operation_id:invocation.operation_id.clone(),
        body_hash:ObjectHash::body(&retained.admission.source_body)?, context:invocation.context.clone(),
    };
    let scoped = TaskScopedAuthorization::for_admitted_operation_in(
        connection, policy, identity, &retained.admission, &facts, now_ms,
    )?;
    Ok((scoped, facts))
}

fn current_task_business_operation_source_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    retained: &RetainedDispatch,
    now_ms: u64,
) -> Result<TaskScopedAuthorization> {
    let invocation = &retained.admission.invocation.payload;
    let (source, scoped) = dispatch_authorization_from(
        connection,
        policy,
        identity,
        invocation.resource.space.as_str(),
        &invocation.operation_id,
        now_ms,
    )?;
    ensure!(
        source == retained.source,
        "Task business submission changed its ready admitted dispatch"
    );
    validate_operation_binding_from(connection, &retained.request)?;
    Ok(scoped)
}

fn current_task_business_submit_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
    retained: &RetainedDispatch,
    now_ms: u64,
) -> Result<(
    TaskScopedAuthorization,
    vibeface_secure::authorization::execution_handoff::VerifiedExecutionHandoff,
)> {
    let scoped =
        current_task_business_operation_source_in(connection, policy, identity, retained, now_ms)?;
    let verified = scoped.state(policy.verified()).verify_execution_handoff(
        wire,
        scoped.authorization(),
        &TransportEvidence::VerifiedPeer {
            origin: vibeface_secure::authorization::DeviceId::from_key(&PublicKey::parse(
                &identity.public_key_hex()?,
            )?),
        },
        None,
        now_ms,
    )?;
    Ok((scoped, verified))
}

impl Store {
    pub(crate) fn current_task_business_operation_source(
        &self,
        identity: &LocalIdentity,
        wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
        now_ms: u64,
    ) -> Result<TaskScopedAuthorization> {
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let retained = retained_dispatch_from(&tx, identity, wire)?;
        let scoped =
            current_task_business_operation_source_in(&tx, &current, identity, &retained, now_ms)?;
        tx.commit()?;
        Ok(scoped)
    }

    pub(crate) fn current_task_business_submit(
        &self,
        identity: &LocalIdentity,
        wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
        now_ms: u64,
    ) -> Result<(
        TaskScopedAuthorization,
        vibeface_secure::authorization::execution_handoff::VerifiedExecutionHandoff,
    )> {
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let retained = retained_dispatch_from(&tx, identity, wire)?;
        let result =
            current_task_business_submit_in(&tx, &current, identity, wire, &retained, now_ms)?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn prepare_execution_dispatch_task_business_source(
        &mut self,
        identity: &LocalIdentity,
        wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
        now_ms: u64,
    ) -> Result<
        Option<(
            super::task_business_policy::TaskBusinessPolicySourceContext,
            vibeface_secure::authorization::execution_handoff::VerifiedExecutionHandoff,
        )>,
    > {
        use super::task_execution_context::OriginalAdmissionView;
        use vibeface_secure::authorization::task_business_policy::{
            TaskBusinessAuthorization, TaskBusinessWire,
        };
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let retained = retained_dispatch_from(&tx, identity, wire)?;
        if OriginalAdmissionView::load(&tx, &current, &retained.admission)?
            .task_policy()
            .is_none()
        {
            tx.commit()?;
            return Ok(None);
        }
        let (scoped, authorization) =
            current_task_business_submit_in(&tx, &current, identity, wire, &retained, now_ms)?;
        let task_policy = scoped
            .task_policy()
            .context("Task business submission has no current execution policy")?
            .clone();
        tx.commit()?;
        self.policy = current;
        let source = self.begin_task_business_source(
            identity,
            &task_policy,
            TaskBusinessAuthorization::Submit(&authorization),
            &TaskBusinessWire::Submit(wire.clone()),
            now_ms,
        )?;
        Ok(Some((source, authorization)))
    }

    pub fn execution_dispatch_handoff(
        &self,
        space: &str,
        operation: &str,
    ) -> Result<Option<vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff>>
    {
        let encoded: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, format!("project-execution-handoff:{operation}")],
                |row| row.get(0),
            )
            .optional()?;
        encoded.map(|encoded| {
            let wire: vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff = serde_json::from_str(&encoded)?;
            let event: crate::model::Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
            ensure!(event.network_id == space && matches!(&event.body, crate::model::EventBody::DispatchRecorded { operation_id, .. } if operation_id == operation),
                "stored execution handoff scope changed");
            Ok(wire)
        }).transpose()
    }

    pub(crate) fn execution_dispatch_uses_task_policy(
        &self,
        identity: &LocalIdentity,
        wire: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
    ) -> Result<bool> {
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let retained = retained_dispatch_from(&tx, identity, wire)?;
        let original = super::task_execution_context::OriginalAdmissionView::load(
            &tx,
            &current,
            &retained.admission,
        )?;
        let uses_task_policy = original.task_policy().is_some();
        tx.commit()?;
        Ok(uses_task_policy)
    }

    pub fn prepare_execution_dispatch_observation(
        &self,
        identity: &LocalIdentity,
        request: &vibeface_secure::authorization::Signed<
            vibeface_secure::authorization::execution_observation::ExecutionObservationRequest,
        >,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<vibeface_secure::authorization::execution_observation::SignedExecutionObservation>
    {
        use vibeface_secure::{
            Identity,
            authorization::{Actor, execution_handoff::SignedExecutionHandoff},
        };
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let invocation = &request.payload.request;
        let observed_presenter = match transport {
            TransportEvidence::DirectNoise { device } => {
                device == &invocation.device && device == &invocation.presenter
            }
            TransportEvidence::GatewayNoise { device, gateway } => {
                device == &invocation.device
                    && gateway == &invocation.presenter
                    && gateway != device
            }
            TransportEvidence::VerifiedPeer { origin } => {
                origin == &invocation.presenter && origin != &invocation.device
            }
            TransportEvidence::LocalAgentAdapter { .. } => false,
        };
        ensure!(
            observed_presenter
                && matches!(invocation.actor, Actor::Account(_))
                && invocation.context.sdk_session.is_none(),
            "execution observation requires the actual signed Noise presenter"
        );
        let target = &request.payload.intent.target;
        let space = target.source.project.space.as_str();
        let operation = &target.source.operation_id;
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let encoded: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, format!("project-execution-handoff:{operation}")],
            |row| row.get(0),
        )?;
        let original: SignedExecutionHandoff = serde_json::from_str(&encoded)?;
        let retained = retained_dispatch_from(&tx, identity, &original)?;
        ensure!(
            super::task_execution_context::OriginalAdmissionView::load(
                &tx,
                &current,
                &retained.admission
            )?
            .task_policy()
            .is_none(),
            "foreign observation requires its dedicated Task readonly policy source"
        );
        ensure!(
            retained.admission.invocation.payload.audience == invocation.audience,
            "observation targets another Project host"
        );
        super::event_authorization::validate_retained_execution_scope_in(
            &tx,
            identity,
            &retained.source,
        )?;
        let verified = current
            .verified()
            .verify_execution_observation_request(request, &original, now_ms)?;
        check_destination_heads_in(&tx, verified.heads())?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let wire = current.verified().sign_execution_observation(
            request,
            &original,
            &signer,
            now_ms,
            verified.expires_at_ms(),
        )?;
        check_destination_heads_in(&tx, &wire.invocation.payload.expected_policies)?;
        tx.commit()?;
        Ok(wire)
    }

    pub(crate) fn prepare_task_execution_dispatch_observation(
        &self,
        identity: &LocalIdentity,
        request: &vibeface_secure::authorization::Signed<
            vibeface_secure::authorization::execution_observation::ExecutionObservationRequest,
        >,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<(
        vibeface_secure::authorization::execution_observation::SignedExecutionObservation,
        vibeface_secure::authorization::node_execution_policy::VerifiedTaskCurrentPolicy,
        vibeface_secure::authorization::execution_observation::VerifiedExecutionObservationCurrent,
    )> {
        use vibeface_secure::{
            Identity,
            authorization::{Actor, execution_handoff::SignedExecutionHandoff},
        };
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let invocation = &request.payload.request;
        let observed_presenter = match transport {
            TransportEvidence::DirectNoise { device } => {
                device == &invocation.device && device == &invocation.presenter
            }
            TransportEvidence::GatewayNoise { device, gateway } => {
                device == &invocation.device
                    && gateway == &invocation.presenter
                    && gateway != device
            }
            TransportEvidence::VerifiedPeer { origin } => {
                origin == &invocation.presenter && origin != &invocation.device
            }
            TransportEvidence::LocalAgentAdapter { .. } => false,
        };
        ensure!(
            observed_presenter
                && matches!(invocation.actor, Actor::Account(_))
                && invocation.context.sdk_session.is_none(),
            "execution observation requires the actual signed Noise presenter"
        );
        let target = &request.payload.intent.target;
        let space = target.source.project.space.as_str();
        let operation = &target.source.operation_id;
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let encoded: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, format!("project-execution-handoff:{operation}")],
            |row| row.get(0),
        )?;
        let original: SignedExecutionHandoff = serde_json::from_str(&encoded)?;
        let retained = retained_dispatch_from(&tx, identity, &original)?;
        ensure!(
            retained.admission.invocation.payload.audience == invocation.audience,
            "observation targets another Project host"
        );
        super::event_authorization::validate_retained_execution_scope_in(
            &tx,
            identity,
            &retained.source,
        )?;
        let binding = retained
            .request
            .binding
            .as_ref()
            .context("observation original has no Task binding")?;
        let scope = super::foreign_execution_policy::current_task_readonly_policy_in(
            &tx, &current, &vibeface_secure::authorization::PublicKey::parse(&identity.public_key_hex()?)?,
            binding,
            &vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicySelection {
                execution_resource: target.execution_resource.clone(),
                target_public_key: target.target_public_key.clone(),
                execution_grant: request.payload.intent.execution_grant.clone(),
            },
            vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose::ObservationRead,
            invocation, &request.issuer,
            &vibeface_secure::authorization::prepare::PrepareContext {
                context: invocation.context.clone(), binding_generation: Some(binding.binding.generation),
                canonical_cwd: None, execution_target_public_key: Some(target.target_public_key.clone()),
            }, now_ms,
        )?;
        let verified = scope
            .state()
            .verify_execution_observation_request(request, &original, now_ms)?;
        super::foreign_execution_policy::check_task_policy_heads_in(&tx, &scope)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let outbox_key = format!("task-business-observation:{}", request.hash()?);
        let saved: Option<String> = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=? AND length(CAST(value AS BLOB))<=?",
            params![space, outbox_key, vibeface_secure::authorization::MAX_PROOF_BYTES], |row| row.get(0),
        ).optional()?;
        let wire = if let Some(saved) = saved {
            let wire: vibeface_secure::authorization::execution_observation::SignedExecutionObservation = serde_json::from_str(&saved)?;
            ensure!(
                wire.request == *request,
                "observation outbox changed its original request"
            );
            wire
        } else {
            let wire = scope.state().sign_execution_observation(
                request,
                &original,
                &signer,
                now_ms,
                verified.expires_at_ms().min(scope.expires_at_ms()),
            )?;
            let encoded = serde_json::to_string(&wire)?;
            ensure!(
                encoded.len() <= vibeface_secure::authorization::MAX_PROOF_BYTES,
                "observation outbox exceeds its carrier bound"
            );
            tx.execute(
                "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                params![space, outbox_key, encoded],
            )?;
            wire
        };
        let strong = scope.state().verify_execution_observation_current(
            &wire,
            &TransportEvidence::VerifiedPeer {
                origin: vibeface_secure::authorization::DeviceId::from_key(
                    &vibeface_secure::authorization::PublicKey::of(&signer),
                ),
            },
            now_ms,
        )?;
        super::foreign_execution_policy::check_task_policy_heads_in(&tx, &scope)?;
        tx.commit()?;
        Ok((wire, scope, strong))
    }

    pub(crate) fn current_task_business_observe(
        &self,
        identity: &LocalIdentity,
        wire: &vibeface_secure::authorization::execution_observation::SignedExecutionObservation,
        now_ms: u64,
    ) -> Result<(
        vibeface_secure::authorization::node_execution_policy::VerifiedTaskCurrentPolicy,
        vibeface_secure::authorization::execution_observation::VerifiedExecutionObservationCurrent,
    )> {
        use vibeface_secure::authorization::{
            DeviceId, PublicKey,
            execution_handoff::SignedExecutionHandoff,
            prepare::PrepareContext,
            remote_execution_policy::{
                RemoteExecutionPolicyPurpose, RemoteExecutionPolicySelection,
            },
        };
        let _signatures =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let request = &wire.request;
        let target = &request.payload.intent.target;
        let space = target.source.project.space.as_str();
        let saved: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=? AND length(CAST(value AS BLOB))<=?",
            params![space,format!("task-business-observation:{}",request.hash()?),vibeface_secure::authorization::MAX_PROOF_BYTES], |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<
                vibeface_secure::authorization::execution_observation::SignedExecutionObservation,
            >(&saved)?
                == *wire,
            "observation differs from its immutable local outbox"
        );
        let encoded: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                space,
                format!("project-execution-handoff:{}", target.source.operation_id)
            ],
            |row| row.get(0),
        )?;
        let original: SignedExecutionHandoff = serde_json::from_str(&encoded)?;
        let retained = retained_dispatch_from(&tx, identity, &original)?;
        super::event_authorization::validate_retained_execution_scope_in(
            &tx,
            identity,
            &retained.source,
        )?;
        let binding = retained
            .request
            .binding
            .as_ref()
            .context("observation original has no Task binding")?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let scope = super::foreign_execution_policy::current_task_readonly_policy_in(
            &tx,
            &current,
            &local,
            binding,
            &RemoteExecutionPolicySelection {
                execution_resource: target.execution_resource.clone(),
                target_public_key: target.target_public_key.clone(),
                execution_grant: request.payload.intent.execution_grant.clone(),
            },
            RemoteExecutionPolicyPurpose::ObservationRead,
            &request.payload.request,
            &request.issuer,
            &PrepareContext {
                context: request.payload.request.context.clone(),
                binding_generation: Some(binding.binding.generation),
                canonical_cwd: None,
                execution_target_public_key: Some(target.target_public_key.clone()),
            },
            now_ms,
        )?;
        scope
            .state()
            .verify_execution_observation_request(request, &original, now_ms)?;
        let strong = scope.state().verify_execution_observation_current(
            wire,
            &TransportEvidence::VerifiedPeer {
                origin: DeviceId::from_key(&local),
            },
            now_ms,
        )?;
        super::foreign_execution_policy::check_task_policy_heads_in(&tx, &scope)?;
        tx.commit()?;
        Ok((scope, strong))
    }

    pub fn prepare_execution_dispatch_handoff(
        &mut self,
        identity: &LocalIdentity,
        space: &str,
        operation: &str,
        now_ms: u64,
    ) -> Result<(
        vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
        bool,
    )> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        use vibeface_secure::{
            Identity,
            authorization::{
                AUTH_VERSION, Actor, DeviceId, Invocation, Nonce, Signed,
                execution_handoff::{SignedExecutionHandoff, execution_submission_facts},
            },
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let (source, scoped) =
            dispatch_authorization_from(&tx, &current, identity, space, operation, now_ms)?;
        let original = scoped.authorization();
        let body: String = tx.query_row(
            "SELECT source_body FROM accepted_authorizations WHERE space_id=? AND operation_id=?",
            params![space, operation],
            |row| row.get(0),
        )?;
        let body: Value = serde_json::from_str(&body)?;
        let dispatch = dispatch_facts_from(
            &tx, scoped.state(current.verified()), &body, original.audience(), now_ms,
        )?;
        dispatch.revalidate(&tx, &current, &scoped, &body, original.audience(), now_ms)?;
        let handoff = super::execution_handoff::execution_handoff_payload_in(
            &tx,
            &current,
            &scoped,
            identity,
            space,
            operation,
            dispatch.destination.resource().clone(),
            dispatch.destination.audience().clone(),
            dispatch.destination.public_key().clone(),
            now_ms,
        )?;
        let facts = execution_submission_facts(&handoff, original)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(original.audience().service.clone()),
                device: DeviceId::from_key(&PublicKey::of(&signer)),
                initiator: dispatch.destination.initiator().clone(),
                initiating_grant: dispatch.destination.initiating_grant().cloned(),
                resource: facts.resource.clone(),
                action: Action::ExecutionSubmit,
                audience: facts.audience.clone(),
                presenter: original.audience().device.clone(),
                operation_id: operation.into(),
                body_hash: facts.body_hash.clone(),
                grant_leaf: dispatch.destination.grant_leaf().clone(),
                expected_policies: dispatch.destination.heads().to_vec(),
                context: facts.context.clone(),
                nonce: Nonce::random()?,
                not_before_ms: now_ms,
                expires_at_ms: now_ms
                    .checked_add(vibeface_secure::authorization::INVOCATION_MAX_AGE_MS)
                    .context("handoff invocation deadline overflow")?
                    .min(scoped.expires_at_ms()),
            },
            &signer,
        )?;
        scoped.state(current.verified()).verify_invocation(
            &invocation,
            &facts,
            &TransportEvidence::VerifiedPeer {
                origin: original.audience().device.clone(),
            },
            now_ms,
        )?;
        let wire = SignedExecutionHandoff {
            handoff,
            invocation,
        };
        let key = format!("project-execution-handoff:{operation}");
        let previous: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: SignedExecutionHandoff = serde_json::from_str(&previous)?;
            ensure!(
                previous.handoff.project_dispatch == source
                    && previous.handoff.execution_resource == *dispatch.destination.resource()
                    && previous.handoff.target_public_key == *dispatch.destination.public_key()
                    && previous.invocation.payload.grant_leaf == *dispatch.destination.grant_leaf(),
                "stored execution handoff differs from its original dispatch"
            );
            tx.commit()?;
            self.policy = current;
            return Ok((previous, false));
        }
        ensure!(tx.execute("UPDATE operations SET state='\"running\"' WHERE space_id=? AND operation_id=? AND state='\"accepted\"'",
            params![space,operation])? == 1,"operation is already uncertain or has completed; reconcile its original attempt");
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![space, key, serde_json::to_string(&wire)?],
        )?;
        tx.commit()?;
        self.policy = current;
        Ok((wire, true))
    }

    pub fn mark_execution_dispatch_unknown(&mut self, space: &str, operation: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_values WHERE space_id=? AND key=?)",
            params![space, format!("project-execution-handoff:{operation}")],
            |row| row.get(0),
        )?;
        ensure!(exists, "operation has no durable execution handoff");
        tx.execute("UPDATE operations SET state='\"unknown\"' WHERE space_id=? AND operation_id=? AND state IN ('\"accepted\"','\"running\"')",
            params![space,operation])?;
        tx.commit()?;
        Ok(())
    }

    pub fn verify_execution_dispatch_receipt(
        &self,
        identity: &LocalIdentity,
        result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult,
        now_ms: u64,
    ) -> Result<()> {
        let _signatures = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx,&self.policy)?;
        let retained = retained_dispatch_from(&tx,identity,&result.handoff)?;
        let (scoped, facts) = retained_dispatch_authorization(&tx,&current,identity,&retained,now_ms)?;
        let verified = scoped.state(current.verified()).verify_execution_handoff_result(result,scoped.authorization(),now_ms)?;
        scoped.check_current_in(&tx,current.verified(),&facts,now_ms)?;
        if scoped.task_policy().is_none() {
            check_policy_heads_in(&tx,current.verified(),verified.original(),now_ms)?;
            check_policy_heads_in(&tx,current.verified(),verified.submission(),now_ms)?;
        }
        validate_operation_binding_from(&tx,&retained.request)?;
        tx.commit()?;
        Ok(())
    }

    pub fn apply_execution_handoff_receipt(
        &mut self,
        identity: &LocalIdentity,
        result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        use vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult;
        let _signatures = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let receipt = &result.receipt.payload;
        let space = receipt.original_space_id.as_str();
        let operation = &receipt.operation_id;
        let state: OperationState = serde_json::from_value(serde_json::to_value(receipt.state)?)?;
        let wrapper = serde_json::to_value(result)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx,&self.policy)?;
        let retained = retained_dispatch_from(&tx,identity,&result.handoff)?;
        let source = &retained.source;
        let saved_wire: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, format!("project-execution-handoff:{operation}")],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<
                vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
            >(&saved_wire)?
                == result.handoff,
            "handoff receipt differs from the durable submitted carrier"
        );
        let (request, old_state, old_result): (String, String, String) = tx.query_row(
            "SELECT request,state,result FROM operations WHERE space_id=? AND operation_id=?",
            params![space, operation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let request: OperationRequest = serde_json::from_str(&request)?;
        let old_state: OperationState = serde_json::from_str(&old_state)?;
        ensure!(
            old_state != OperationState::Unknown,
            "an uncertain Project operation requires explicit Account resolution"
        );
        let old_result: Value = serde_json::from_str(&old_result)?;
        let source_event: crate::model::Event = serde_json::from_str(&source.canonical_body)?;
        ensure!(
            matches!(&source_event.body, crate::model::EventBody::DispatchRecorded { action, .. }
            if *action == serde_json::to_value(&request)?),
            "operation differs from its admitted dispatch"
        );
        if old_state == state && old_result == wrapper {
            let invocation = &retained.admission.invocation.payload;
            let facts = RequestFacts {
                resource: invocation.resource.clone(), action: invocation.action,
                audience: invocation.audience.clone(), operation_id: invocation.operation_id.clone(),
                body_hash: ObjectHash::body(&retained.admission.source_body)?, context: invocation.context.clone(),
            };
            super::admission::validate_persisted_admission(&tx, &retained.admission)?;
            super::admission::reconstruct_stored_admission(
                &tx, &current, &retained.admission, &facts,
            )?;
            if old_state.terminal() {
                crate::store::validate_released_operation_reservations_from(&tx,space,operation)?;
            } else {
                validate_operation_binding_from(&tx, &request)?;
                super::admission::check_operation_reservations_in(&tx,&OperationRecord {
                    request:request.clone(),state:old_state.clone(),result:old_result.clone(),
                })?;
            }
            tx.commit()?;
            self.policy = current;
            return Ok(OperationRecord { request, state: old_state, result: old_result });
        }
        let (scoped, facts) = retained_dispatch_authorization(&tx,&current,identity,&retained,now_ms)?;
        let verified = scoped.state(current.verified()).verify_execution_handoff_result(result,scoped.authorization(),now_ms)?;
        scoped.check_current_in(&tx,current.verified(),&facts,now_ms)?;
        if scoped.task_policy().is_none() {
            check_policy_heads_in(&tx,current.verified(),verified.original(),now_ms)?;
            check_policy_heads_in(&tx,current.verified(),verified.submission(),now_ms)?;
        }
        validate_operation_binding_from(&tx, &request)?;
        let previous = (!old_result.is_null())
            .then(|| serde_json::from_value::<ExecutionHandoffResult>(old_result.clone()))
            .transpose()?;
        if let Some(previous) = &previous {
            ensure!(
                previous.handoff == result.handoff
                    && previous.receipt.payload.issued_at_ms <= receipt.issued_at_ms,
                "execution acknowledgement moved backwards or changed its carrier"
            );
            if old_state == state && previous.result == result.result {
                if old_state.terminal() {
                    crate::store::validate_released_operation_reservations_from(&tx,space,operation)?;
                } else {
                    super::admission::check_operation_reservations_in(&tx,&OperationRecord {
                        request:request.clone(),state:old_state.clone(),result:old_result.clone(),
                    })?;
                }
                tx.execute(
                    "UPDATE operations SET result=? WHERE space_id=? AND operation_id=?",
                    params![serde_json::to_string(&wrapper)?, space, operation],
                )?;
                tx.commit()?;
                self.policy = current;
                return Ok(OperationRecord {
                    request,
                    state: old_state,
                    result: wrapper,
                });
            }
        }
        ensure!(
            !old_state.terminal(),
            "terminal execution receipt cannot change"
        );
        ensure!(
            state != OperationState::Accepted
                || previous.is_none()
                || old_state == OperationState::Accepted,
            "execution acknowledgement moved backwards"
        );
        super::admission::check_operation_reservations_in(&tx,&OperationRecord {
            request:request.clone(),state:old_state,result:old_result.clone(),
        })?;
        tx.execute(
            "UPDATE operations SET state=?,result=? WHERE space_id=? AND operation_id=?",
            params![
                serde_json::to_string(&state)?,
                serde_json::to_string(&wrapper)?,
                space,
                operation
            ],
        )?;
        if state != OperationState::Accepted {
            super::event_authorization::append_execution_handoff_observation_in(
                &tx,
                &current,
                identity,
                source,
                result,
                &verified,
                now_ms,
            )?;
        }
        if state.terminal() {
            tx.execute(
                "DELETE FROM resource_reservations WHERE space_id=? AND operation_id=?",
                params![space, operation],
            )?;
        }
        tx.commit()?;
        self.policy = current;
        Ok(OperationRecord {
            request,
            state,
            result: wrapper,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use anyhow::Result;
    use vibeface_secure::authorization::*;

    pub(crate) struct Fixture {
        pub(crate) project: TestNetwork,
        pub(crate) executor: TestNetwork,
        pub(crate) store: Store,
        pub(crate) execution: ResourceRef,
        pub(crate) grant: ObjectHash,
        pub(crate) task: crate::model::SignedEvent,
        pub(crate) audience: Audience,
        pub(crate) journal: Vec<JournalEntry>,
    }

    impl Fixture {
        pub(crate) fn new() -> Result<Self> {
            Self::with_task_creator(false)
        }

        pub(crate) fn new_with_foreign_account_task() -> Result<Self> {
            Self::with_task_creator(true)
        }

        pub(crate) fn new_same_account_nodes() -> Result<Self> {
            let project = TestNetwork::new()?;
            let executor = project.same_account_node()?;
            Self::from_networks(project, executor, false)
        }

        fn with_task_creator(foreign: bool) -> Result<Self> {
            Self::from_networks(TestNetwork::new()?, TestNetwork::new()?, foreign)
        }

        pub(crate) fn from_networks(project: TestNetwork, executor: TestNetwork, foreign: bool) -> Result<Self> {
            let mut store = project.at(&project.directory.join("project.sqlite3"))?;
            if project.account == executor.account {
                store.install_policy_journal(&executor.bundle.journal, now())?;
            } else {
                executor.install(&mut store)?;
            }
            project.conversation(&mut store, "conversation")?;
            let (task, mut task_policies) = if foreign {
                Self::foreign_account_task(&project, &executor, &mut store)?
            } else {
                (
                    project.task(&mut store, "conversation", "remote-task")?,
                    Vec::new(),
                )
            };
            let key = PublicKey::parse(&project.node.public_key_hex()?)?;
            let audience = Audience {
                service: ServiceId::from_key(&key),
                device: DeviceId::from_key(&key),
            };
            let target_key = PublicKey::parse(&executor.node.public_key_hex()?)?;
            let target = Audience {
                service: ServiceId::from_key(&target_key),
                device: DeviceId::from_key(&target_key),
            };
            let execution = ResourceRef {
                space: executor.space.clone(),
                resource: ResourceId::from_bytes([91; 32]),
            };
            let stamp = store
                .policy
                .verified()
                .head(&PolicyDomain::Space(executor.space.clone()))
                .unwrap()
                .clone();
            let resource = Signed::sign(
                ResourceRecord {
                    version: AUTH_VERSION,
                    space: executor.space.clone(),
                    resource: execution.resource.clone(),
                    kind: ResourceKind::ExecutionService,
                    host: target.service.clone(),
                    issued_under: stamp.clone(),
                    nonce: Nonce::random()?,
                },
                &executor.root,
            )?;
            let parent = Signed::sign(
                Grant {
                    execution_source: None,
                    version: AUTH_VERSION,
                    owner_space: executor.space.clone(),
                    scope: ResourceScope::Exact(execution.clone()),
                    grantee: Grantee::Actor(Actor::Account(executor.account.clone())),
                    actions: vec![
                        Action::ExecutionOpen,
                        Action::ExecutionSend,
                        Action::ExecutionSubmit,
                        Action::ExecutionResolve,
                        Action::OperationStatusRead,
                        Action::OperationResolve,
                    ],
                    audience: target,
                    constraints: GrantConstraints {
                        context: InvocationContext {
                            conversation: Some("conversation".into()),
                            task: Some(EventRef::parse(&task.hash)?),
                            execution_session: Some(SessionRef {
                                target: DeviceId::from_key(&target_key),
                                session_id: "remote-session".into(),
                            }),
                            executor_kind: Some("codex".into()),
                            workspace_resource: Some(execution.resource.clone()),
                            sdk_session: None,
                        },
                        ..Default::default()
                    },
                    parent: None,
                    remaining_delegations: 1,
                    issued_under: stamp.clone(),
                    not_before_ms: now(),
                    expires_at_ms: now() + 1_000_000,
                    nonce: Nonce::random()?,
                },
                &executor.root,
            )?;
            let mut document = store
                .policy
                .verified()
                .space(&executor.space)
                .unwrap()
                .clone();
            document.previous = Some(stamp);
            document.sequence += 1;
            document.resources.push(resource.hash()?);
            document.resources.sort();
            document.grants.push(parent.hash()?);
            document.grants.sort();
            task_policies.extend([
                JournalEntry::Resource(resource),
                JournalEntry::Grant(parent.clone()),
                JournalEntry::Space(Signed::sign(document, &executor.root)?),
            ]);
            let mut journal = task_policies;
            store.install_policy_journal(&journal, now())?;
            let stamp = store
                .policy
                .verified()
                .head(&PolicyDomain::Space(executor.space.clone()))
                .unwrap()
                .clone();
            let mut child = parent.payload.clone();
            child.actions.retain(|action| {
                !matches!(action, Action::ExecutionResolve | Action::OperationResolve)
            });
            child.grantee = Grantee::Actor(Actor::Service(audience.service.clone()));
            child.parent = Some(parent.hash()?);
            child.remaining_delegations = 0;
            child.issued_under = stamp.clone();
            child.nonce = Nonce::random()?;
            let child = Signed::sign(child, &executor.client)?;
            let grant = child.hash()?;
            let mut document = store
                .policy
                .verified()
                .space(&executor.space)
                .unwrap()
                .clone();
            document.previous = Some(stamp);
            document.sequence += 1;
            document.grants.push(grant.clone());
            document.grants.sort();
            let tail = vec![
                JournalEntry::Grant(child),
                JournalEntry::Space(Signed::sign(document, &executor.root)?),
            ];
            store.install_policy_journal(&tail, now())?;
            journal.extend(tail);
            Ok(Self {
                project,
                executor,
                store,
                execution,
                grant,
                task,
                audience,
                journal,
            })
        }

        pub(crate) fn foreign_account_task(
            project: &TestNetwork,
            executor: &TestNetwork,
            store: &mut Store,
        ) -> Result<(crate::model::SignedEvent, Vec<JournalEntry>)> {
            let domain = PolicyDomain::Space(project.space.clone());
            let stamp = store.policy.verified().head(&domain).unwrap().clone();
            let node_key = PublicKey::parse(&project.node.public_key_hex()?)?;
            let audience = Audience {
                service: ServiceId::from_key(&node_key),
                device: DeviceId::from_key(&node_key),
            };
            let grant = Signed::sign(
                Grant {
                    execution_source: None,
                    version: AUTH_VERSION,
                    owner_space: project.space.clone(),
                    scope: ResourceScope::Exact(ResourceRef {
                        space: project.space.clone(),
                        resource: project.resource.clone(),
                    }),
                    grantee: Grantee::Actor(Actor::Account(executor.account.clone())),
                    actions: vec![Action::TaskCreate],
                    audience: audience.clone(),
                    constraints: Default::default(),
                    parent: None,
                    remaining_delegations: 0,
                    issued_under: stamp.clone(),
                    not_before_ms: now(),
                    expires_at_ms: now() + 1_000_000,
                    nonce: Nonce::random()?,
                },
                &project.root,
            )?;
            let leaf = grant.hash()?;
            let mut document = store
                .policy
                .verified()
                .space(&project.space)
                .unwrap()
                .clone();
            document.previous = Some(stamp);
            document.sequence += 1;
            document.grants.push(leaf.clone());
            document.grants.sort();
            let journal = vec![
                JournalEntry::Grant(grant),
                JournalEntry::Space(Signed::sign(document, &project.root)?),
            ];
            store.install_policy_journal(&journal, now())?;
            let mut body = project.body("create_task", "conversation");
            body["task_id"] = json!("remote-task");
            body["title"] = json!("remote-task");
            body["goal"] = json!("work");
            body["acceptance"] = json!("review result");
            body["auto_followup"] = json!(false);
            let facts = store.request_facts(&body, &audience)?;
            let device = DeviceId::from_key(&PublicKey::of(&executor.client));
            let mut heads = [
                PolicyDomain::Account(project.account.clone()),
                PolicyDomain::Account(executor.account.clone()),
                domain,
            ]
            .iter()
            .map(|domain| store.policy.verified().head(domain).unwrap().clone())
            .collect::<Vec<_>>();
            heads.sort_by(|left, right| left.domain.cmp(&right.domain));
            heads.dedup();
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(executor.account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(executor.account.clone()),
                    initiating_grant: None,
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience,
                    presenter: device.clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: leaf,
                    expected_policies: heads,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 100_000,
                },
                &executor.client,
            )?;
            let transport = TransportEvidence::DirectNoise { device };
            let auth = store.authorize_request(&invocation, &facts, &transport, now())?;
            let challenge = store.issue_freshness(&auth, CheckStage::Admit, now())?;
            let mut receipts = store.sign_current_receipts(&challenge, &project.node, now())?;
            receipts.extend(store.sign_current_receipts(&challenge, &executor.node, now())?);
            let proof = FreshnessProof {
                challenge,
                receipts,
            };
            let event = store.append_authorized_event(
                &project.node,
                &auth,
                &proof,
                &transport,
                &body,
                "conversation",
                crate::model::EventBody::TaskCreated {
                    task_id: "remote-task".into(),
                    title: "remote-task".into(),
                    goal: "work".into(),
                    acceptance: "review result".into(),
                },
                Vec::new(),
                now() as i64,
            )?;
            Ok((event, journal))
        }

        pub(crate) fn selection(&self, method: &str, id: &str) -> Result<Value> {
            Ok(
                json!({"method":method,"space_id":self.project.space,"resource_id":self.project.resource,
                "authorization_id":id,"execution_resource":self.execution,"execution_grant":self.grant,
                "target_public_key":self.executor.node.public_key_hex()?}),
            )
        }

        fn resolver_grant(&mut self) -> Result<ObjectHash> {
            let parent = self
                .store
                .policy
                .verified()
                .grant(&self.grant)
                .unwrap()
                .payload
                .parent
                .clone()
                .unwrap();
            let mut child = self
                .store
                .policy
                .verified()
                .grant(&parent)
                .unwrap()
                .payload
                .clone();
            let stamp = self
                .store
                .policy
                .verified()
                .head(&PolicyDomain::Space(self.executor.space.clone()))
                .unwrap()
                .clone();
            child.parent = Some(parent);
            child.grantee = Grantee::Actor(Actor::Service(self.audience.service.clone()));
            child.actions = vec![Action::ExecutionResolve, Action::OperationResolve];
            child.remaining_delegations = 0;
            child.issued_under = stamp.clone();
            child.nonce = Nonce::random()?;
            let child = Signed::sign(child, &self.executor.client)?;
            let hash = child.hash()?;
            let mut document = self
                .store
                .policy
                .verified()
                .space(&self.executor.space)
                .unwrap()
                .clone();
            document.previous = Some(stamp);
            document.sequence += 1;
            document.grants.push(hash.clone());
            document.grants.sort();
            let tail = vec![
                JournalEntry::Grant(child),
                JournalEntry::Space(Signed::sign(document, &self.executor.root)?),
            ];
            self.store.install_policy_journal(&tail, now() + 3)?;
            self.journal.extend(tail);
            Ok(hash)
        }

        pub(crate) fn authorize(
            &mut self,
            body: &Value,
        ) -> Result<(
            VerifiedDispatchFacts,
            VerifiedAuthorization,
            FreshnessProof,
            TransportEvidence,
        )> {
            self.authorize_at(body, now())
        }

        fn authorize_at(
            &mut self,
            body: &Value,
            at: u64,
        ) -> Result<(
            VerifiedDispatchFacts,
            VerifiedAuthorization,
            FreshnessProof,
            TransportEvidence,
        )> {
            let signer =
                vibeface_secure::Identity::from_secret_hex(&self.project.client.secret_hex())
                    .map_err(anyhow::Error::msg)?;
            let account = self.project.account.clone();
            let leaf = self.project.bundle.grant_leaf.clone();
            self.authorize_as(body, at, &signer, &account, &leaf)
        }

        fn authorize_as(
            &mut self,
            body: &Value,
            at: u64,
            signer: &vibeface_secure::Identity,
            account: &AccountId,
            leaf: &ObjectHash,
        ) -> Result<(
            VerifiedDispatchFacts,
            VerifiedAuthorization,
            FreshnessProof,
            TransportEvidence,
        )> {
            let dispatch = self
                .store
                .execution_dispatch_facts(body, &self.audience, at)?;
            let facts = dispatch.facts();
            let device = DeviceId::from_key(&PublicKey::of(signer));
            let mut heads = vec![
                self.store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Account(self.project.account.clone()))
                    .unwrap()
                    .clone(),
                self.store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Account(self.executor.account.clone()))
                    .unwrap()
                    .clone(),
                self.store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Space(self.project.space.clone()))
                    .unwrap()
                    .clone(),
            ];
            heads.extend_from_slice(dispatch.destination().heads());
            heads.sort_by(|left, right| left.domain.cmp(&right.domain));
            heads.dedup();
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(account.clone()),
                    initiating_grant: None,
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience: self.audience.clone(),
                    presenter: device.clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: leaf.clone(),
                    expected_policies: heads,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: at,
                    expires_at_ms: at + 100_000,
                },
                signer,
            )?;
            let transport = TransportEvidence::DirectNoise { device };
            let auth = self
                .store
                .authorize_request_for_body(&invocation, facts, &transport, body, at)?;
            let challenge = self.store.issue_freshness(&auth, CheckStage::Admit, at)?;
            let mut receipts =
                self.store
                    .sign_current_receipts(&challenge, &self.project.node, at)?;
            receipts.extend(self.store.sign_current_receipts(
                &challenge,
                &self.executor.node,
                at,
            )?);
            Ok((
                dispatch,
                auth,
                FreshnessProof {
                    challenge,
                    receipts,
                },
                transport,
            ))
        }

        pub(crate) fn bind(&mut self) -> Result<SignedBinding> {
            self.bind_at(self.project.project.to_str().unwrap().to_owned())
        }

        pub(crate) fn bind_at(&mut self, cwd: String) -> Result<SignedBinding> {
            let mut body = self.selection("bind_execution", "bind-remote")?;
            for (field, value) in [
                ("conversation_id", json!("conversation")),
                ("task_id", json!("remote-task")),
                ("task_ref", json!(self.task.hash)),
                ("target_node", json!(self.executor.node.node_id()?)),
                ("session_id", json!("remote-session")),
                ("executor", json!("codex")),
                ("cwd", json!(cwd)),
            ] {
                body[field] = value;
            }
            let (dispatch, auth, proof, transport) = self.authorize(&body)?;
            self.store.bind_execution_dispatch(
                &self.project.node,
                &dispatch,
                &TaskScopedAuthorization::from_primary(auth.clone()),
                &proof,
                &transport,
                &body,
                now(),
            )
        }

        pub(crate) fn operation(&self, binding: SignedBinding, id: &str) -> Result<(Value, OperationRequest)> {
            let request = OperationRequest {
                operation_id: id.into(),
                network_id: self.project.space.to_string(),
                conversation_id: "conversation".into(),
                task_id: Some("remote-task".into()),
                target_node: self.executor.node.node_id()?,
                session_id: "remote-session".into(),
                expected_revision: 0,
                basis_heads: vec![self.task.hash.clone()],
                action: binding.binding.open_spec.clone(),
                resources: vec![binding.binding.open_spec["cwd"].as_str().unwrap().into()],
                binding: Some(binding),
            };
            let mut body = self.selection("operate", id)?;
            body["request"] = serde_json::to_value(&request)?;
            Ok((body, request))
        }

        fn confirmed_project_client(&mut self, at: u64) -> Result<VerifiedClientAuthorization> {
            super::super::client_policy_read::verified_client_policy_for_test(
                &mut self.store,
                &self.project.node,
                &self.project.client,
                &self.project.account,
                vibeface_secure::authorization::client_policy_import::AuthorizationSelection {
                    resource: ResourceRef {
                        space: self.project.space.clone(),
                        resource: self.project.resource.clone(),
                    },
                    audience: self.audience.clone(),
                    grant_leaf: self.project.bundle.grant_leaf.clone(),
                },
                &self.project.pins,
                at,
            )
        }

        fn imported_execution_policy(
            &mut self,
            source: &VerifiedClientAuthorization,
            selection: &Value,
            context: &vibeface_secure::authorization::prepare::PrepareContext,
            action: Action,
            purpose: vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose,
            independent_pins: &[PolicyPin],
            at: u64,
        ) -> Result<
            vibeface_secure::authorization::remote_execution_policy::VerifiedRemoteExecutionPolicy,
        > {
            use vibeface_secure::authorization::remote_execution_policy::{
                RemoteExecutionPolicyPage, begin_remote_execution_policy_import,
                build_remote_execution_policy_read_intent,
            };
            let principal = self.project.client.public_key_hex();
            let target = self.executor.node.public_key_hex()?;
            let intent = build_remote_execution_policy_read_intent(
                source,
                &json!({"selection":selection,"context":context,"expected_action":action,
                    "purpose":purpose,"execution_pins":independent_pins})
                .to_string(),
                &self.project.client,
                at as f64,
                (at + 60_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let delegation = self.store.remote_execution_policy_delegate(
                &self.project.node,
                &principal,
                &serde_json::from_str(&intent)?,
                at,
            )?;
            let request = json!({"intent":serde_json::from_str::<Value>(&intent)?,
                "delegation":delegation["delegation"]});
            let mut c = self.executor.memory()?;
            c.install_policy_journal(&self.journal, at)?;
            let prepared =
                c.remote_execution_policy_prepare(&self.executor.node, &principal, &request, at)?;
            assert!(prepared.get("authorization").is_none());
            let mut importer = begin_remote_execution_policy_import(
                source,
                &request.to_string(),
                &prepared["snapshot"].to_string(),
                &serde_json::to_string(independent_pins)?,
                &target,
                at as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let mut page_count = 0;
            loop {
                let params = serde_json::from_str(
                    &importer.page_request_json().map_err(anyhow::Error::msg)?,
                )?;
                let response =
                    c.remote_execution_policy_page(&self.executor.node, &principal, &params, at)?;
                assert!(response.get("authorization").is_none());
                let page: Signed<RemoteExecutionPolicyPage> =
                    serde_json::from_value(response["page"].clone())?;
                importer
                    .append_page(&serde_json::to_string(&page)?, at as f64)
                    .map_err(anyhow::Error::msg)?;
                page_count += 1;
                ensure!(
                    page_count <= 4096,
                    "execution policy fixture did not complete"
                );
                if page.payload.complete {
                    break;
                }
            }
            let params =
                serde_json::from_str(&importer.page_request_json().map_err(anyhow::Error::msg)?)?;
            let response =
                c.remote_execution_policy_finish(&self.executor.node, &principal, &params, at)?;
            assert!(response.get("authorization").is_none());
            importer
                .finish(&response["witness"].to_string(), at as f64)
                .map_err(anyhow::Error::msg)
        }
    }

    #[test]
    fn same_account_nodes_keep_independent_execution_keys_and_native_proofs() -> Result<()> {
        use vibeface_secure::authorization::prepare::*;
        let mut f = Fixture::new_same_account_nodes()?;
        assert_eq!(f.project.account, f.executor.account);
        assert_ne!(
            f.project.node.public_key_hex()?,
            f.executor.node.public_key_hex()?
        );
        let binding = f.bind_at(f.executor.project.to_str().unwrap().to_owned())?;
        let client = f.confirmed_project_client(now() + 1)?;
        let selection = json!({"execution_resource":f.execution,"target_public_key":f.executor.node.public_key_hex()?,"execution_grant":f.grant});
        let context = PrepareContext {
            context: f
                .store
                .policy
                .verified()
                .grant(&f.grant)
                .unwrap()
                .payload
                .constraints
                .context
                .clone(),
            binding_generation: Some(1),
            canonical_cwd: binding.binding.open_spec["cwd"].as_str().map(str::to_owned),
            execution_target_public_key: Some(PublicKey::parse(
                &f.executor.node.public_key_hex()?,
            )?),
        };
        let mut context = context;
        context.context.workspace_resource = Some(f.project.resource.clone());
        let independent = f.store.trusted_policy_pins().to_vec();
        let execution = f.imported_execution_policy(&client, &selection, &context,
            Action::ExecutionOpen,
            vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose::Execution,
            &independent, now() + 1)?;
        let handle = verify_remote_execution(&client, &execution, (now() + 1) as f64)
            .map_err(anyhow::Error::msg)?;
        let (body, request) = f.operation(binding.clone(), "same-account-remote-open")?;
        let prepared: Value = serde_json::from_str(
            &handle
                .prepare(
                    &client,
                    &body.to_string(),
                    &serde_json::to_string(&context)?,
                    "execution_open",
                    "same-account-remote-open",
                    &f.project.node.public_key_hex()?,
                    &f.project.client,
                    (now() + 1) as f64,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        assert_eq!(
            prepared["payload"]["request"]["context"]["execution_session"]["target"],
            serde_json::to_value(DeviceId::from_key(&PublicKey::parse(
                &f.executor.node.public_key_hex()?
            )?))?
        );
        let (facts, auth, fresh, transport) = f.authorize_at(&body, now() + 1)?;
        f.store.accept_execution_dispatch(
            &f.project.node,
            &facts,
            &TaskScopedAuthorization::from_primary(auth.clone()),
            &fresh,
            &transport,
            &body,
            &request,
            now() + 1,
        )?;
        let (wire, _) = f.store.prepare_execution_dispatch_handoff(
            &f.project.node,
            f.project.space.as_str(),
            &request.operation_id,
            now() + 1,
        )?;
        assert_eq!(
            wire.handoff.target_public_key.as_str(),
            f.executor.node.public_key_hex()?
        );
        Ok(())
    }

    #[test]
    fn foreign_task_fixture_keeps_signed_creator_and_remote_binding() -> Result<()> {
        let mut f = Fixture::new_with_foreign_account_task()?;
        let event: crate::model::Event = serde_json::from_str(&f.task.canonical_body)?;
        let crate::model::EventCause::Invocation { admission } = event.cause else {
            anyhow::bail!("foreign Task fixture must have an actual admitted Account creation");
        };
        assert_eq!(admission.invocation.payload.actor, Actor::Account(f.executor.account.clone()));
        assert_ne!(admission.invocation.payload.actor, Actor::Account(f.project.account.clone()));
        let binding = f.bind_at(f.executor.project.to_str().unwrap().to_owned())?;
        assert_eq!(binding.binding.task_ref, f.task.hash);
        assert_eq!(binding.binding.target_node, f.executor.node.node_id()?);
        assert_eq!(f.store.policy.verified().grant(&f.grant).unwrap().payload.constraints.context.task,
            Some(EventRef::parse(&f.task.hash)?));
        Ok(())
    }

    #[test]
    fn signed_remote_results_and_metadata_keep_project_and_executor_state_separate() -> Result<()> {
        use vibeface_secure::authorization::{execution_handoff::*, prepare::*};
        let mut f = Fixture::new()?;
        let binding = f.bind()?;
        let (body, request) = f.operation(binding.clone(), "remote-result")?;
        let (dispatch, auth, proof, transport) = f.authorize(&body)?;
        f.store.accept_execution_dispatch(
            &f.project.node,
            &dispatch,
            &TaskScopedAuthorization::from_primary(auth.clone()),
            &proof,
            &transport,
            &body,
            &request,
            now(),
        )?;
        let (wire, fresh) = f.store.prepare_execution_dispatch_handoff(
            &f.project.node,
            f.project.space.as_str(),
            &request.operation_id,
            now(),
        )?;
        assert!(fresh);
        let (same, fresh) = f.store.prepare_execution_dispatch_handoff(
            &f.project.node,
            f.project.space.as_str(),
            &request.operation_id,
            now(),
        )?;
        assert!(!fresh);
        assert_eq!(same, wire);
        let mut c = f.executor.memory()?;
        f.project.install(&mut c)?;
        c.install_policy_journal(&f.journal, now())?;
        c.local_set(
            f.executor.space.as_str(),
            &format!("resource-root:{}", f.execution.resource),
            &json!(f.project.project),
        )?;
        let peer = TransportEvidence::VerifiedPeer {
            origin: f.audience.device.clone(),
        };
        let challenge =
            c.issue_execution_handoff_challenge(&wire, &peer, CheckStage::Admit, now())?;
        let mut receipts = c.sign_execution_handoff_receipts(&challenge, &f.project.node, now())?;
        receipts.extend(c.sign_execution_handoff_receipts(&challenge, &f.executor.node, now())?);
        let admit = ExecutionHandoffFreshness {
            challenge,
            receipts,
        };
        c.accept_execution_handoff(&f.executor.node, &wire, &admit, &peer, now())?;
        let current = c.execution_handoff_authorization(
            f.execution.space.as_str(),
            f.project.space.as_str(),
            &request.operation_id,
            now(),
        )?;
        let challenge =
            c.issue_execution_handoff_challenge(&wire, &peer, CheckStage::Start, now() + 1)?;
        let mut receipts =
            c.sign_execution_handoff_receipts(&challenge, &f.project.node, now() + 1)?;
        receipts.extend(c.sign_execution_handoff_receipts(
            &challenge,
            &f.executor.node,
            now() + 1,
        )?);
        let (running, fresh, _) = c.claim_execution_handoff(
            &f.executor.node,
            &current,
            &ExecutionHandoffFreshness {
                challenge,
                receipts,
            },
            now() + 1,
        )?;
        assert!(fresh);
        let mut session = running.execution_session.context("C session is missing")?;
        session.state = "idle".into();
        let done = c.finish_execution_handoff(
            &f.executor.node,
            f.execution.space.as_str(),
            f.project.space.as_str(),
            &request.operation_id,
            OperationState::Succeeded,
            json!({"opened":true}),
            Some(&session),
            now() + 2,
        )?;
        let receipt = done.receipt.context("C result receipt is missing")?;
        let result = ExecutionHandoffResult {
            handoff: wire.clone(),
            receipt: receipt.clone(),
            result: done.result,
        };
        f.store
            .verify_execution_dispatch_receipt(&f.project.node, &result, now() + 2)?;
        let count: u64 =
            f.store
                .connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        let mut wrong = result.clone();
        wrong.result = json!({"opened":false});
        assert!(
            f.store
                .apply_execution_handoff_receipt(&f.project.node, &wrong, now() + 2)
                .is_err()
        );
        assert_eq!(
            f.store
                .operation(f.project.space.as_str(), &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Running
        );
        f.store
            .apply_execution_handoff_receipt(&f.project.node, &result, now() + 2)?;
        assert_eq!(
            f.store.connection.query_row(
                "SELECT COUNT(*) FROM resource_reservations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        assert_eq!(
            f.store
                .connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, u64>(0))?,
            count + 1
        );
        f.store
            .apply_execution_handoff_receipt(&f.project.node, &result, now() + 2)?;
        assert_eq!(
            f.store
                .connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, u64>(0))?,
            count + 1
        );
        assert!(
            f.store
                .session(
                    f.project.space.as_str(),
                    &request.target_node,
                    &request.session_id
                )?
                .is_none()
        );
        let client = f.confirmed_project_client(now())?;
        let raw = PrepareContext {
            context: dispatch.facts.context.clone(),
            binding_generation: Some(1),
            canonical_cwd: Some(f.project.project.to_str().unwrap().into()),
            execution_target_public_key: Some(PublicKey::parse(
                &f.executor.node.public_key_hex()?,
            )?),
        };
        let mut cb = f.executor.bundle.clone();
        cb.journal.extend(f.journal.clone());
        cb.grant_leaf = f.grant.clone();
        let selection = json!({"execution_resource":f.execution,"execution_grant":f.grant,"target_public_key":f.executor.node.public_key_hex()?});
        let independent = f.executor.pins.clone();
        let execution = f.imported_execution_policy(&client, &selection, &raw,
            Action::ExecutionOpen,
            vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose::Execution,
            &independent, now())?;
        let handle = verify_remote_execution(&client, &execution, now() as f64)
            .map_err(anyhow::Error::msg)?;
        let verified_result: Value = serde_json::from_str(
            &handle
                .result_json(
                    &serde_json::to_string(&result)?,
                    &body.to_string(),
                    &serde_json::to_string(auth.signed_invocation())?,
                    &serde_json::to_string(&binding)?,
                    (now() + 2) as f64,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        assert_eq!(verified_result["result"], json!({"opened":true}));
        assert!(
            handle
                .result_json(
                    &serde_json::to_string(&wrong)?,
                    &body.to_string(),
                    &serde_json::to_string(auth.signed_invocation())?,
                    &serde_json::to_string(&binding)?,
                    (now() + 2) as f64
                )
                .is_err()
        );
        let verified_observation: Value = serde_json::from_str(
            &handle
                .observation_json(
                    &serde_json::to_string(&result)?,
                    &serde_json::to_string(&binding)?,
                    (now() + 2) as f64,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        assert_eq!(verified_observation["result"], json!({"opened":true}));
        let resolver_grant = f.resolver_grant()?;
        let mut resolver_bundle = f.executor.bundle.clone();
        resolver_bundle.journal.extend(f.journal.clone());
        resolver_bundle.grant_leaf = resolver_grant.clone();
        let resolver_selection = json!({"execution_resource":f.execution,"execution_grant":resolver_grant,
            "target_public_key":f.executor.node.public_key_hex()?});
        let resolver_policy = f.imported_execution_policy(&client, &resolver_selection, &raw,
            Action::OperationResolve,
            vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose::Resolver,
            &independent, now() + 3)?;
        let resolver = verify_remote_execution(&client, &resolver_policy, (now() + 3) as f64)
            .map_err(anyhow::Error::msg)?;
        let intent: Value = serde_json::from_str(
            &resolver
                .resolution_body_json(
                    &serde_json::to_string(&result)?,
                    "resolve-terminal",
                    "succeeded",
                    "verified the original executor result",
                    (now() + 3) as f64,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        assert_eq!(intent["body"]["resolution_purpose"], "reconcile_terminal");
        assert_eq!(
            intent["body"]["execution_observation"],
            serde_json::to_value(&result)?
        );
        assert!(
            resolver
                .resolution_body_json(
                    &serde_json::to_string(&result)?,
                    "resolve-terminal",
                    "failed",
                    "verified the original executor result",
                    (now() + 3) as f64
                )
                .is_err()
        );
        let signed_prepare: Signed<PrepareIntent> = serde_json::from_str(
            &resolver
                .prepare(
                    &client,
                    &intent["body"].to_string(),
                    &intent["context"].to_string(),
                    "operation_resolve",
                    "resolve-terminal",
                    &f.project.node.public_key_hex()?,
                    &f.project.client,
                    (now() + 3) as f64,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        let prepared = f.store.policy.verified().verify_prepare_intent(
            &signed_prepare,
            &intent["body"],
            Action::OperationResolve,
            &f.audience,
            &transport,
            now() + 3,
        )?;
        let execution_domain = PolicyDomain::Space(f.execution.space.clone());
        let execution_head = f.store.policy.verified().head(&execution_domain).unwrap();
        assert!(signed_prepare.payload.request.expected_policies.contains(execution_head));
        let mut missing_execution_head = signed_prepare.payload.clone();
        missing_execution_head.request.expected_policies.retain(|head| head.domain != execution_domain);
        assert!(matches!(
            f.store.policy.verified().verify_prepare_intent(
                &Signed::sign(missing_execution_head, &f.project.client)?,
                &intent["body"],
                Action::OperationResolve,
                &f.audience,
                &transport,
                now() + 3,
            ),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let signed_resolution: Signed<Invocation> = serde_json::from_str(
            &resolver.sign(
                &client,
                &serde_json::to_string(&signed_prepare.payload.request)?,
                &intent["body"].to_string(),
                "operation_resolve",
                "resolve-terminal",
                &f.project.node.public_key_hex()?,
                &f.project.client,
                (now() + 3) as f64,
            ).map_err(anyhow::Error::msg)?,
        )?;
        let resolution_facts = RequestFacts {
            resource: signed_resolution.payload.resource.clone(),
            action: Action::OperationResolve,
            audience: f.audience.clone(),
            operation_id: "resolve-terminal".into(),
            body_hash: ObjectHash::body(&intent["body"])?,
            context: signed_resolution.payload.context.clone(),
        };
        let destination = prepared.execution_destination().context("resolver destination missing")?;
        let verified_resolution = f.store.policy.verified().verify_invocation_with_execution_destination(
            &signed_resolution, &resolution_facts, &transport, destination, now() + 3,
        )?;
        assert!(verified_resolution.dependencies().contains(execution_head));
        let mut missing_execution_head = signed_resolution.payload;
        missing_execution_head.expected_policies.retain(|head| head.domain != execution_domain);
        assert!(matches!(
            f.store.policy.verified().verify_invocation_with_execution_destination(
                &Signed::sign(missing_execution_head, &f.project.client)?,
                &resolution_facts, &transport, destination, now() + 3,
            ),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let resolver_facts =
            f.store
                .execution_dispatch_facts(&intent["body"], &f.audience, now() + 3)?;
        assert_eq!(
            resolver_facts.facts().context,
            signed_prepare.payload.request.context
        );
        let mut share_body = f.project.body("execution_share_request", "conversation");
        share_body["task_id"] = json!("remote-task");
        share_body["task_ref"] = json!(f.task.hash);
        share_body["share_request"] = json!({"purpose":"execution","session_id":"second-session","executor_kind":"codex",
            "expires_at_ms":now()+60_000});
        let (share_auth, _) = f.project.authorization(&f.store, &share_body, now() + 3)?;
        let share_input = f.store.execution_share_request(
            &f.project.node,
            &share_auth,
            &share_body,
            now() + 3,
        )?;
        let task_event: crate::model::Event = serde_json::from_str(&f.task.canonical_body)?;
        let compact: remote_share::RemoteShareIntent = serde_json::from_value(share_input.clone())?;
        compact.task_creation.verify_signature()?;
        assert_eq!(compact.task_creation.issuer, compact.source_node_public_key);
        assert_eq!(
            compact.task_creation.payload.task,
            EventRef::parse(&f.task.hash)?
        );
        assert_eq!(compact.task_creation.payload.project, compact.project);
        assert_eq!(compact.task_creation.payload.task_id, "remote-task");
        assert_eq!(
            compact.task_creation.payload.conversation_id,
            task_event.scope_id
        );
        assert_eq!(compact.task_creation.payload.author, task_event.author);
        assert_eq!(compact.task_creation.payload.sequence, task_event.sequence);
        assert_eq!(compact.requester, f.project.account);
        assert_eq!(
            compact.requester_device,
            share_auth.signed_invocation().payload.device
        );
        assert_eq!(
            compact.source_node_public_key.as_str(),
            f.project.node.public_key_hex()?
        );
        assert!(serde_json::to_vec(&share_input)?.len() <= MAX_PROOF_BYTES);
        assert!(share_input.get("request").is_none());
        assert!(share_input.get("bundle").is_none());
        assert!(share_input.get("pins").is_none());
        if std::env::var_os("VIBEFACE_EXPORT_REMOTE_FIXTURE").is_some() {
            use std::os::unix::fs::OpenOptionsExt;
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.artifacts/remote-execution-client-fixture.json");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(path)?;
            serde_json::to_writer(
                file,
                &json!({"nowMs":now()+3,"accountId":f.project.account,"spaceId":f.project.space,
                "testClientSecret":f.project.client.secret_hex(),"clientPublicKey":PublicKey::of(&f.project.client),
                "projectPublicKey":f.project.node.public_key_hex()?,"projectBundle":f.project.bundle,"projectPins":f.project.pins,
                "executionBundle":cb,"executionPins":f.executor.pins,"selection":selection,"context":raw,"binding":binding,
                "browserBody":body,"browserInvocation":auth.signed_invocation(),"actual":result,"receipt":receipt,
                "resolverBundle":resolver_bundle,"resolverSelection":resolver_selection,"resolverIntent":intent,
                "resolverPrepare":signed_prepare,"shareIntentInput":share_input}),
            )?;
        }
        let snapshot: Value = serde_json::from_str(
            &handle
                .session_json(
                    &serde_json::to_string(&receipt)?,
                    &serde_json::to_string(&binding)?,
                    (now() + 2) as f64,
                )
                .map_err(anyhow::Error::msg)?,
        )?;
        assert_eq!(snapshot["revision"], 1);
        assert!(snapshot.get("native_session_id").is_none());
        assert!(
            handle
                .session_json(
                    &serde_json::to_string(&receipt)?,
                    &serde_json::to_string(&binding)?,
                    (now() + 100_000) as f64
                )
                .is_err()
        );
        let source = wire.handoff.source_attestation.as_ref().unwrap();
        let read = json!({"method":"operation","space_id":f.project.space,"resource_id":f.project.resource,
            "authorization_id":"read-remote","operation_id":request.operation_id,"execution_source":source,"binding":binding});
        f.executor.revoke_node(&mut f.store)?;
        let facts = f
            .store
            .execution_dispatch_metadata_facts(&read, &f.audience, now() + 3)?
            .unwrap();
        assert!(facts.context.execution_session.is_none());
        assert!(facts.context.executor_kind.is_none());
        let context = verify_project_execution_metadata(
            &client,
            &serde_json::to_string(source)?,
            &serde_json::to_string(&binding)?,
        )
        .map_err(anyhow::Error::msg)?;
        let signed = sign_client_prepare_intent(
            &client,
            &read.to_string(),
            &context,
            "operation_status_read",
            "read-remote",
            &f.project.node.public_key_hex()?,
            &f.project.client,
            (now() + 3) as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let signed: Signed<PrepareIntent> = serde_json::from_str(&signed)?;
        assert!(
            signed
                .payload
                .request
                .expected_policies
                .iter()
                .all(|head| head.domain != PolicyDomain::Account(f.executor.account.clone()))
        );
        f.store.policy.verified().verify_prepare_intent(
            &signed,
            &read,
            Action::OperationStatusRead,
            &f.audience,
            &transport,
            now() + 3,
        )?;
        let mut forged = read;
        forged["operation_id"] = json!("unknown-operation");
        assert!(
            f.store
                .execution_dispatch_metadata_facts(&forged, &f.audience, now() + 3)
                .is_err()
        );
        let mut share_store = f.project.memory()?;
        f.executor.install(&mut share_store)?;
        f.project.conversation(&mut share_store, "conversation")?;
        let large_goal = "g".repeat(MAX_PROOF_BYTES + 1);
        let mut large_body = f.project.body("create_task", "conversation");
        large_body["task_id"] = json!("large-share-task");
        large_body["title"] = json!("large-share-task");
        large_body["goal"] = json!(large_goal);
        large_body["acceptance"] = json!("review result");
        large_body["auto_followup"] = json!(false);
        let large_task = f.project.append(
            &mut share_store,
            &large_body,
            crate::model::EventBody::TaskCreated {
                task_id: "large-share-task".into(),
                title: "large-share-task".into(),
                goal: large_goal,
                acceptance: "review result".into(),
            },
            now() + 4,
        )?;
        assert!(large_task.canonical_body.len() > MAX_PROOF_BYTES);
        let stamp = share_store
            .policy
            .verified()
            .head(&PolicyDomain::Space(f.project.space.clone()))
            .unwrap()
            .clone();
        let read_grant = Signed::sign(
            Grant {
                execution_source: None,
                version: AUTH_VERSION,
                owner_space: f.project.space.clone(),
                scope: ResourceScope::Exact(compact.project),
                grantee: Grantee::Actor(Actor::Account(f.executor.account.clone())),
                actions: vec![Action::ConversationRead],
                audience: f.audience.clone(),
                constraints: Default::default(),
                parent: None,
                remaining_delegations: 0,
                issued_under: stamp.clone(),
                not_before_ms: now(),
                expires_at_ms: now() + 60_000,
                nonce: Nonce::random()?,
            },
            &f.project.root,
        )?;
        let leaf = read_grant.hash()?;
        let mut document = share_store
            .policy
            .verified()
            .space(&f.project.space)
            .unwrap()
            .clone();
        document.previous = Some(stamp);
        document.sequence += 1;
        document.grants.push(leaf.clone());
        document.grants.sort();
        share_store.install_policy_journal(
            &[
                JournalEntry::Grant(read_grant),
                JournalEntry::Space(Signed::sign(document, &f.project.root)?),
            ],
            now() + 4,
        )?;
        share_body["authorization_id"] = json!("large-task-share-input");
        share_body["task_id"] = json!("large-share-task");
        share_body["task_ref"] = json!(large_task.hash);
        let facts = share_store.request_facts(&share_body, &f.audience)?;
        let device = DeviceId::from_key(&PublicKey::of(&f.executor.client));
        let mut heads = [
            PolicyDomain::Account(f.project.account.clone()),
            PolicyDomain::Account(f.executor.account.clone()),
            PolicyDomain::Space(f.project.space.clone()),
        ]
        .iter()
        .map(|domain| share_store.policy.verified().head(domain).unwrap().clone())
        .collect::<Vec<_>>();
        heads.sort_by(|left, right| left.domain.cmp(&right.domain));
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(f.executor.account.clone()),
                device: device.clone(),
                initiator: Actor::Account(f.executor.account.clone()),
                initiating_grant: None,
                resource: facts.resource.clone(),
                action: facts.action,
                audience: f.audience.clone(),
                presenter: device.clone(),
                operation_id: facts.operation_id.clone(),
                body_hash: facts.body_hash.clone(),
                grant_leaf: leaf,
                expected_policies: heads,
                context: facts.context.clone(),
                nonce: Nonce::random()?,
                not_before_ms: now() + 4,
                expires_at_ms: now() + 60_000,
            },
            &f.executor.client,
        )?;
        let auth = share_store.authorize_request(
            &invocation,
            &facts,
            &TransportEvidence::DirectNoise { device },
            now() + 4,
        )?;
        let large_input =
            share_store.execution_share_request(&f.project.node, &auth, &share_body, now() + 4)?;
        assert!(serde_json::to_vec(&large_input)?.len() <= MAX_PROOF_BYTES);
        let compact: remote_share::RemoteShareIntent = serde_json::from_value(large_input)?;
        compact.task_creation.verify_signature()?;
        assert_eq!(compact.task_creation.issuer, compact.source_node_public_key);
        assert_eq!(
            compact.task_creation.payload.task,
            EventRef::parse(&large_task.hash)?
        );
        assert_eq!(compact.task_creation.payload.task_id, "large-share-task");
        assert_eq!(
            compact.task_creation.payload.conversation_id,
            "conversation"
        );
        assert_eq!(compact.requester, f.executor.account);
        assert_ne!(compact.requester, f.project.account);
        assert_eq!(compact.requester_device, invocation.payload.device);
        assert_eq!(
            compact.source_node_public_key.as_str(),
            f.project.node.public_key_hex()?
        );
        Ok(())
    }

    #[test]
    fn remote_client_signatures_derive_cross_account_heads_from_independent_pins() -> Result<()> {
        use vibeface_secure::authorization::prepare::{
            PrepareContext, PrepareIntent, verify_remote_execution,
        };
        let mut f = Fixture::new()?;
        let remote_cwd = "/vibeface-remote-only/nonexistent-project";
        assert!(!std::path::Path::new(remote_cwd).exists());
        let binding = f.bind_at(remote_cwd.into())?;
        let (body, _) = f.operation(binding, "remote-client-open")?;
        let dispatch = f
            .store
            .execution_dispatch_facts(&body, &f.audience, now())?;
        let raw = PrepareContext {
            context: dispatch.facts().context.clone(),
            binding_generation: Some(1),
            canonical_cwd: Some(remote_cwd.into()),
            execution_target_public_key: Some(PublicKey::parse(
                &f.executor.node.public_key_hex()?,
            )?),
        };
        let auth = f.confirmed_project_client(now())?;
        let selection = json!({"execution_resource":f.execution,"target_public_key":f.executor.node.public_key_hex()?,"execution_grant":f.grant});
        let independent = f.executor.pins.clone();
        let mut wrong_pins = independent.clone();
        wrong_pins[0].genesis_root = PublicKey::of(&f.project.root);
        let mut verify = |pins: &[PolicyPin]| -> Result<_> {
            let policy = f.imported_execution_policy(&auth, &selection, &raw,
                Action::ExecutionOpen,
                vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose::Execution,
                pins, now())?;
            verify_remote_execution(&auth, &policy, now() as f64).map_err(anyhow::Error::msg)
        };
        assert!(verify(&[]).is_err());
        assert!(verify(&wrong_pins).is_err());
        let handle = verify(&independent)?;
        let prepared = handle
            .prepare(
                &auth,
                &body.to_string(),
                &serde_json::to_string(&raw)?,
                "execution_open",
                "remote-client-open",
                &f.project.node.public_key_hex()?,
                &f.project.client,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
        let prepared: Signed<PrepareIntent> = serde_json::from_str(&prepared)?;
        assert!(
            prepared
                .payload
                .request
                .expected_policies
                .iter()
                .any(|head| head.domain == PolicyDomain::Account(f.executor.account.clone()))
        );
        assert!(
            !prepared
                .payload
                .request
                .expected_policies
                .iter()
                .any(|head| head.domain == PolicyDomain::Space(f.executor.space.clone()))
        );
        let transport = TransportEvidence::DirectNoise {
            device: auth.scope().device_id.clone(),
        };
        f.store.policy.verified().verify_prepare_intent(
            &prepared,
            &body,
            Action::ExecutionOpen,
            &f.audience,
            &transport,
            now(),
        )?;
        let signed = handle
            .sign(
                &auth,
                &serde_json::to_string(&prepared.payload.request)?,
                &body.to_string(),
                "execution_open",
                "remote-client-open",
                &f.project.node.public_key_hex()?,
                &f.project.client,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
        let signed: Signed<Invocation> = serde_json::from_str(&signed)?;
        f.store
            .authorize_request(&signed, dispatch.facts(), &transport, now())?;
        Ok(())
    }

    #[test]
    fn remote_project_admission_is_atomic_and_does_not_create_a_local_executor_session()
    -> Result<()> {
        let mut f = Fixture::new()?;
        assert!(
            f.store
                .authorize_node(f.project.space.as_str(), &f.executor.node.node_id()?, None)
                .is_err()
        );
        let binding = f.bind()?;
        let (body, request) = f.operation(binding, "remote-open")?;
        assert!(f.store.request_facts(&body, &f.audience).is_err());
        let mut wrong = body.clone();
        wrong["target_public_key"] = json!(f.project.node.public_key_hex()?);
        assert!(
            f.store
                .execution_dispatch_facts(&wrong, &f.audience, now())
                .is_err()
        );
        let (dispatch, auth, proof, transport) = f.authorize(&body)?;
        let before: u64 = f.store.connection.query_row(
            "SELECT COUNT(*) FROM authorization_nonces",
            [],
            |row| row.get(0),
        )?;
        f.store.connection.execute_batch("CREATE TEMP TRIGGER fail_remote_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'forced remote event rollback'); END;")?;
        assert!(
            f.store
                .accept_execution_dispatch(
                    &f.project.node,
                    &dispatch,
                    &TaskScopedAuthorization::from_primary(auth.clone()),
                    &proof,
                    &transport,
                    &body,
                    &request,
                    now()
                )
                .is_err()
        );
        assert!(
            f.store
                .operation(f.project.space.as_str(), &request.operation_id)?
                .is_none()
        );
        assert_eq!(
            f.store.connection.query_row(
                "SELECT COUNT(*) FROM resource_reservations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        assert_eq!(
            f.store.connection.query_row(
                "SELECT COUNT(*) FROM authorization_nonces",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            before
        );
        f.store
            .connection
            .execute_batch("DROP TRIGGER fail_remote_event;")?;
        let accepted = f.store.accept_execution_dispatch(
            &f.project.node,
            &dispatch,
            &TaskScopedAuthorization::from_primary(auth.clone()),
            &proof,
            &transport,
            &body,
            &request,
            now(),
        )?;
        assert_eq!(accepted.state, OperationState::Accepted);
        assert!(
            f.store
                .session(
                    f.project.space.as_str(),
                    &request.target_node,
                    &request.session_id
                )?
                .is_none()
        );
        assert_eq!(
            f.store.connection.query_row(
                "SELECT COUNT(*) FROM authorization_nonces",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            before + 1
        );
        assert_eq!(
            f.store
                .accept_execution_dispatch(
                    &f.project.node,
                    &dispatch,
                    &TaskScopedAuthorization::from_primary(auth.clone()),
                    &proof,
                    &transport,
                    &body,
                    &request,
                    now()
                )?
                .state,
            OperationState::Accepted
        );
        Ok(())
    }
}
