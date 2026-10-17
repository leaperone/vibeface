use super::{
    admission::{check_policy_heads_in, consume_execution_dispatch_in, validate_basis_in},
    journal::PolicyState,
    policy::{FreshnessProof, request_execution_dispatch_facts_from},
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
    prepare::VerifiedExecutionDestination,
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
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<RequestFacts> {
        let current = dispatch_facts_from(connection, policy.verified(), body, audience, now_ms)?;
        ensure!(
            current.facts == self.facts
                && current.destination.grant_leaf() == self.destination.grant_leaf()
                && current.destination.heads() == self.destination.heads(),
            "execution destination changed before admission"
        );
        check_destination_heads_in(connection, current.destination.heads())?;
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

pub(super) fn check_destination_heads_in(connection: &Connection, heads: &[PolicyStamp]) -> Result<()> {
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
        matches!(body["method"].as_str(), Some("operate" | "bind_execution" | "resolve_unknown")),
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
    let facts =
        request_execution_dispatch_facts_from(connection, policies, body, audience, &destination)?;
    Ok(VerifiedDispatchFacts { facts, destination })
}

impl Store {
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

    pub fn bind_execution_dispatch(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<SignedBinding> {
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
        dispatch.revalidate(&tx, &self.policy, body, auth.audience(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth, now_ms)?;
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, current_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            ensure!(
                serde_json::from_str::<String>(&current)? == hash,
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
            self.policy.verified().verify_freshness(
                auth,
                &proof.challenge,
                &proof.receipts,
                now_ms,
            )?;
            tx.commit()?;
            return Ok(signed);
        }
        consume_execution_dispatch_in(
            &tx,
            &self.policy,
            auth,
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
        tx.commit()?;
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
    pub fn accept_execution_dispatch(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        request: &OperationRequest,
        now_ms: u64,
    ) -> Result<OperationRecord> {
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
        let facts = dispatch.revalidate(&tx, &self.policy, body, auth.audience(), now_ms)?;
        self.policy.verified().verify_invocation(
            auth.signed_invocation(),
            &facts,
            transport,
            now_ms,
        )?;
        check_policy_heads_in(&tx, self.policy.verified(), auth, now_ms)?;
        self.policy
            .verified()
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
            return Ok(OperationRecord {
                request: request.clone(),
                state: serde_json::from_str(&state)?,
                result: serde_json::from_str(&result)?,
            });
        }
        validate_basis_in(&tx, request)?;
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
        let admission = consume_execution_dispatch_in(
            &tx,
            &self.policy,
            auth,
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
        super::event_authorization::append_admitted_dispatch_in(
            &tx, identity, &admission, request, now_ms,
        )?;
        tx.commit()?;
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
    space: &str,
    operation: &str,
    now_ms: u64,
) -> Result<(crate::model::SignedEvent, VerifiedAuthorization)> {
    let events = crate::store::load_events_from(connection, space)?;
    let mut matching = events.values().filter(|stored| {
        matches!(&stored.event.body,
        crate::model::EventBody::DispatchRecorded { operation_id, .. } if operation_id == operation)
    });
    let source = matching
        .next()
        .context("operation has no admitted dispatch")?;
    ensure!(
        matching.next().is_none() && source.status == "ready",
        "operation has conflicting or incomplete dispatches"
    );
    let auth = super::event_authorization::current_event_authorization(
        policy,
        connection,
        &source.event,
        &events,
        now_ms,
    )?;
    Ok((source.envelope.clone(), auth))
}

impl Store {
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
        use vibeface_secure::{
            Identity,
            authorization::{
                AUTH_VERSION, Actor, DeviceId, Invocation, Nonce, Signed,
                execution_handoff::{SignedExecutionHandoff, execution_submission_facts},
            },
        };
        let (source, original) =
            dispatch_authorization_from(&self.connection, &self.policy, space, operation, now_ms)?;
        let body: String = self.connection.query_row(
            "SELECT source_body FROM accepted_authorizations WHERE space_id=? AND operation_id=?",
            params![space, operation],
            |row| row.get(0),
        )?;
        let body: Value = serde_json::from_str(&body)?;
        let dispatch = self.execution_dispatch_facts(&body, original.audience(), now_ms)?;
        let handoff = self.execution_handoff_payload(
            identity,
            space,
            operation,
            dispatch.destination.resource().clone(),
            dispatch.destination.audience().clone(),
            dispatch.destination.public_key().clone(),
            now_ms,
        )?;
        let facts = execution_submission_facts(&handoff, &original)?;
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
                    .context("handoff invocation deadline overflow")?,
            },
            &signer,
        )?;
        self.policy.verified().verify_invocation(
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
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        dispatch.revalidate(&tx, &self.policy, &body, original.audience(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), &original, now_ms)?;
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
            return Ok((previous, false));
        }
        ensure!(tx.execute("UPDATE operations SET state='\"running\"' WHERE space_id=? AND operation_id=? AND state='\"accepted\"'",
            params![space,operation])? == 1,"operation is already uncertain or has completed; reconcile its original attempt");
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![space, key, serde_json::to_string(&wire)?],
        )?;
        tx.commit()?;
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
        let receipt = &result.receipt.payload;
        let space = receipt.original_space_id.as_str();
        let operation = &receipt.operation_id;
        let (source, original) =
            dispatch_authorization_from(&self.connection, &self.policy, space, operation, now_ms)?;
        ensure!(
            result.handoff.handoff.project_dispatch == source
                && vibeface_secure::authorization::DeviceId::from_key(&PublicKey::parse(
                    &identity.public_key_hex()?
                )?) == original.audience().device,
            "handoff observation belongs to another Project dispatch or host"
        );
        ensure!(
            self.execution_dispatch_handoff(space, operation)?.as_ref() == Some(&result.handoff),
            "handoff observation differs from its durable submitted carrier"
        );
        let verified = self.policy.verified().verify_execution_handoff_ack(
            &result.handoff,
            &original,
            &result.receipt,
            &result.result,
            now_ms,
        )?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            verified.original(),
            now_ms,
        )?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            verified.submission(),
            now_ms,
        )?;
        let stored = self
            .operation(space, operation)?
            .context("observed operation is missing")?;
        let event: crate::model::Event = serde_json::from_str(&source.canonical_body)?;
        ensure!(
            matches!(&event.body,crate::model::EventBody::DispatchRecorded {action,..}
            if *action == serde_json::to_value(&stored.request)?),
            "observed operation differs from its accepted dispatch"
        );
        Ok(())
    }

    pub fn apply_execution_handoff_receipt(
        &mut self,
        identity: &LocalIdentity,
        result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        use vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult;
        let receipt = &result.receipt.payload;
        let space = receipt.original_space_id.as_str();
        let operation = &receipt.operation_id;
        let (source, original) =
            dispatch_authorization_from(&self.connection, &self.policy, space, operation, now_ms)?;
        ensure!(
            result.handoff.handoff.project_dispatch == source
                && vibeface_secure::authorization::DeviceId::from_key(&PublicKey::parse(
                    &identity.public_key_hex()?
                )?) == original.audience().device,
            "handoff result belongs to another Project dispatch or host"
        );
        let verified = self.policy.verified().verify_execution_handoff_ack(
            &result.handoff,
            &original,
            &result.receipt,
            &result.result,
            now_ms,
        )?;
        let state: OperationState = serde_json::from_value(serde_json::to_value(receipt.state)?)?;
        let wrapper = serde_json::to_value(result)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_policy_heads_in(&tx, self.policy.verified(), verified.original(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), verified.submission(), now_ms)?;
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
                tx.execute(
                    "UPDATE operations SET result=? WHERE space_id=? AND operation_id=?",
                    params![serde_json::to_string(&wrapper)?, space, operation],
                )?;
                tx.commit()?;
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
                &self.policy,
                identity,
                &source,
                result,
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
        Ok(OperationRecord {
            request,
            state,
            result: wrapper,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use anyhow::Result;
    use vibeface_secure::authorization::*;

    struct Fixture {
        project: TestNetwork,
        executor: TestNetwork,
        store: Store,
        execution: ResourceRef,
        grant: ObjectHash,
        task: crate::model::SignedEvent,
        audience: Audience,
        journal: Vec<JournalEntry>,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let project = TestNetwork::new()?;
            let executor = TestNetwork::new()?;
            let mut store = project.at(&project.directory.join("project.sqlite3"))?;
            project.conversation(&mut store, "conversation")?;
            let task = project.task(&mut store, "conversation", "remote-task")?;
            executor.install(&mut store)?;
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
                    version: AUTH_VERSION,
                    owner_space: executor.space.clone(),
                    scope: ResourceScope::Exact(execution.clone()),
                    grantee: Grantee::Actor(Actor::Account(executor.account.clone())),
                    actions: vec![
                        Action::ExecutionOpen,
                        Action::ExecutionSend,
                        Action::ExecutionSubmit,
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
            let mut journal = vec![
                JournalEntry::Resource(resource),
                JournalEntry::Grant(parent.clone()),
                JournalEntry::Space(Signed::sign(document, &executor.root)?),
            ];
            store.install_policy_journal(&journal, now())?;
            let stamp = store
                .policy
                .verified()
                .head(&PolicyDomain::Space(executor.space.clone()))
                .unwrap()
                .clone();
            let mut child = parent.payload.clone();
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

        fn selection(&self, method: &str, id: &str) -> Result<Value> {
            Ok(
                json!({"method":method,"space_id":self.project.space,"resource_id":self.project.resource,
                "authorization_id":id,"execution_resource":self.execution,"execution_grant":self.grant,
                "target_public_key":self.executor.node.public_key_hex()?}),
            )
        }

        fn authorize(
            &mut self,
            body: &Value,
        ) -> Result<(
            VerifiedDispatchFacts,
            VerifiedAuthorization,
            FreshnessProof,
            TransportEvidence,
        )> {
            let dispatch = self
                .store
                .execution_dispatch_facts(body, &self.audience, now())?;
            let facts = dispatch.facts();
            let device = DeviceId::from_key(&PublicKey::of(&self.project.client));
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
            heads.sort_by(|left, right| left.domain.cmp(&right.domain));
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(self.project.account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(self.project.account.clone()),
                    initiating_grant: None,
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience: self.audience.clone(),
                    presenter: device.clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: self.project.bundle.grant_leaf.clone(),
                    expected_policies: heads,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 100_000,
                },
                &self.project.client,
            )?;
            let transport = TransportEvidence::DirectNoise { device };
            let auth = self
                .store
                .authorize_request(&invocation, facts, &transport, now())?;
            let challenge = self
                .store
                .issue_freshness(&auth, CheckStage::Admit, now())?;
            let mut receipts =
                self.store
                    .sign_current_receipts(&challenge, &self.project.node, now())?;
            receipts.extend(self.store.sign_current_receipts(
                &challenge,
                &self.executor.node,
                now(),
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

        fn bind(&mut self) -> Result<SignedBinding> {
            self.bind_at(self.project.project.to_str().unwrap().to_owned())
        }

        fn bind_at(&mut self, cwd: String) -> Result<SignedBinding> {
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
                &auth,
                &proof,
                &transport,
                &body,
                now(),
            )
        }

        fn operation(&self, binding: SignedBinding, id: &str) -> Result<(Value, OperationRequest)> {
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
            &auth,
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
        let client = verify_client_authorization(
            &serde_json::to_string(&f.project.bundle)?,
            &serde_json::to_string(&f.project.pins)?,
            f.project.account.as_str(),
            f.project.space.as_str(),
            PublicKey::of(&f.project.client).as_str(),
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
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
        let handle = verify_remote_execution(
            &client,
            &serde_json::to_string(&f.project.bundle)?,
            &serde_json::to_string(&f.project.pins)?,
            &serde_json::to_string(&cb)?,
            &serde_json::to_string(&f.executor.pins)?,
            &selection.to_string(),
            &serde_json::to_string(&raw)?,
            "execution_open",
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
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
        let auth = verify_client_authorization(
            &serde_json::to_string(&f.project.bundle)?,
            &serde_json::to_string(&f.project.pins)?,
            f.project.account.as_str(),
            f.project.space.as_str(),
            PublicKey::of(&f.project.client).as_str(),
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let mut c_bundle = f.executor.bundle.clone();
        c_bundle.journal.extend(f.journal.clone());
        c_bundle.grant_leaf = f.grant.clone();
        let selection = json!({"execution_resource":f.execution,"target_public_key":f.executor.node.public_key_hex()?,"execution_grant":f.grant});
        let verify = |pins: &[PolicyPin]| {
            verify_remote_execution(
                &auth,
                &serde_json::to_string(&f.project.bundle).unwrap(),
                &serde_json::to_string(&f.project.pins).unwrap(),
                &serde_json::to_string(&c_bundle).unwrap(),
                &serde_json::to_string(pins).unwrap(),
                &selection.to_string(),
                &serde_json::to_string(&raw).unwrap(),
                "execution_open",
                now() as f64,
            )
        };
        assert!(verify(&[]).is_err());
        let mut wrong_pins = f.executor.pins.clone();
        wrong_pins[0].genesis_root = PublicKey::of(&f.project.root);
        assert!(verify(&wrong_pins).is_err());
        let handle = verify(&f.executor.pins).map_err(anyhow::Error::msg)?;
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
                    &auth,
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
            &auth,
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
                    &auth,
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
