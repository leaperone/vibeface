use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use vibeface_secure::authorization::AccountId;
use vibeface_secure::authorization::bootstrap::{self, NodeClaim};
use vibeface_secure::authorization::gateway_forward::{GatewayOriginal, VerifiedDirectedGateway};
use vibeface_secure::authorization::{
    Actor, Audience, CheckStage, ClientAuthorizationBundle, DeviceId, FreshnessChallenge,
    FreshnessReceipt, Invocation, InvocationContext, JournalEntry, ObjectHash, PolicyDomain,
    PolicyPin, PublicKey, RequestFacts, ResourceId, ResourceKind, ResourceRef, ServiceId,
    SessionRef, Signed, SpaceId, TransportEvidence, VerifiedAuthorization, VerifiedPolicySet,
};

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum RequestTransport {
    Existing(TransportEvidence),
    DirectedGateway {
        verified: VerifiedDirectedGateway,
        original: GatewayOriginal,
    },
}

impl RequestTransport {
    pub(crate) fn stored(&self) -> crate::model::StoredTransport {
        match self {
            Self::Existing(evidence) => crate::model::StoredTransport::from(evidence),
            Self::DirectedGateway { verified, .. } => {
                crate::model::StoredTransport::DirectedGateway {
                    header: verified.header().clone(),
                }
            }
        }
    }

    pub(crate) fn existing(&self) -> Option<&TransportEvidence> {
        match self {
            Self::Existing(evidence) => Some(evidence),
            Self::DirectedGateway { .. } => None,
        }
    }

    pub(crate) fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Existing(_) => u64::MAX,
            Self::DirectedGateway { verified, .. } => verified.header().payload.expires_at_ms,
        }
    }

    pub(crate) fn check_current(&self, auth: &VerifiedAuthorization, now_ms: u64) -> Result<()> {
        let Self::DirectedGateway { verified, original } = self else {
            return Ok(());
        };
        verified.check_original(original, &verified.header().payload.target_key, now_ms)?;
        let (invocation, body) = match original {
            GatewayOriginal::Invoke { invocation, body }
            | GatewayOriginal::Status { invocation, body } => (invocation, body),
            GatewayOriginal::Prepare { .. } => {
                anyhow::bail!("prepare transport cannot authorize an invocation")
            }
        };
        ensure!(
            invocation == auth.signed_invocation()
                && ObjectHash::body(body)? == *auth.body_hash()
                && matches!(auth.actor(), Actor::Account(_)),
            "directed gateway transport changed its original Account authorization"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshnessProof {
    pub challenge: FreshnessChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
}

pub(crate) fn execution_destination_for_body(
    state: &VerifiedPolicySet,
    facts: &RequestFacts,
    body: &Value,
    now_ms: u64,
) -> Result<Option<vibeface_secure::authorization::prepare::VerifiedExecutionDestination>> {
    let remote_method = matches!(
        body["method"].as_str(),
        Some(
            "operate"
                | "bind_execution"
                | "rebind_execution"
                | "claim_execution"
                | "resolve_unknown"
        )
    );
    if !remote_method
        || ["execution_resource", "target_public_key", "execution_grant"]
            .iter()
            .all(|field| body.get(*field).is_none())
    {
        return Ok(None);
    }
    let resource: ResourceRef = serde_json::from_value(body["execution_resource"].clone())?;
    let key: PublicKey = serde_json::from_value(body["target_public_key"].clone())?;
    let grant: ObjectHash = serde_json::from_value(body["execution_grant"].clone())?;
    Ok(Some(state.verify_execution_destination(
        &facts.resource,
        &facts.audience,
        &resource,
        &key,
        &grant,
        &facts.context,
        facts.action,
        now_ms,
    )?))
}

pub(crate) fn authorize_invocation_for_body(
    state: &VerifiedPolicySet,
    invocation: &Signed<Invocation>,
    facts: &RequestFacts,
    transport: &TransportEvidence,
    body: &Value,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    authorize_request_invocation_for_body(
        state,
        invocation,
        facts,
        &RequestTransport::Existing(transport.clone()),
        body,
        now_ms,
    )
}

pub(crate) fn authorize_request_invocation_for_body(
    state: &VerifiedPolicySet,
    invocation: &Signed<Invocation>,
    facts: &RequestFacts,
    transport: &RequestTransport,
    body: &Value,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    let _verification = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
    invocation.verify_signature()?;
    ensure!(
        facts.body_hash == ObjectHash::body(body)?
            && body["space_id"].as_str() == Some(facts.resource.space.as_str())
            && body["resource_id"].as_str() == Some(facts.resource.resource.as_str())
            && body["authorization_id"].as_str() == Some(facts.operation_id.as_str())
            && crate::model::authorization_action(body)? == facts.action,
        "request body differs from its parsed authorization facts"
    );
    let destination = execution_destination_for_body(state, facts, body, now_ms)?
        .map(|destination| destination.bind_request(&invocation.payload, body))
        .transpose()?;
    if let Some(destination) = &destination {
        destination.check_facts(facts)?;
    }
    let authorization = match transport {
        RequestTransport::Existing(evidence) => match destination {
            Some(destination) => state.verify_invocation_with_execution_destination(
                invocation,
                facts,
                evidence,
                &destination,
                now_ms,
            )?,
            None => state.verify_invocation(invocation, facts, evidence, now_ms)?,
        },
        RequestTransport::DirectedGateway { verified, original } => {
            ensure!(
                original.body() == body,
                "directed gateway changed its raw business body"
            );
            match destination {
                Some(destination) => state.verify_gateway_invocation_with_execution_destination(
                    invocation,
                    body,
                    facts,
                    verified,
                    &destination,
                    now_ms,
                )?,
                None => {
                    state.verify_gateway_invocation(invocation, body, facts, verified, now_ms)?
                }
            }
        }
    };
    transport.check_current(&authorization, now_ms)?;
    Ok(authorization)
}

pub(crate) fn request_facts_from(
    connection: &Connection,
    policies: &VerifiedPolicySet,
    body: &Value,
    audience: &Audience,
) -> Result<RequestFacts> {
    if matches!(
        body["method"].as_str(),
        Some("seal_remote_execution" | "remote_execution_seal_status")
    ) {
        return super::execution_binding_seal::facts_from(
            connection,
            policies,
            body,
            audience,
            crate::runtime::now_ms() as u64,
        )
        .map(|(facts, _)| facts);
    }
    ensure!(
        ["execution_resource", "target_public_key", "execution_grant"]
            .iter()
            .all(|field| body.get(*field).is_none()),
        "remote execution requires a verified destination"
    );
    request_facts_impl(connection, policies, body, audience, None)
}

pub(crate) fn request_execution_dispatch_facts_from(
    connection: &Connection,
    policies: &VerifiedPolicySet,
    body: &Value,
    audience: &Audience,
    destination: &vibeface_secure::authorization::prepare::VerifiedExecutionDestination,
    now_ms: u64,
) -> Result<RequestFacts> {
    if body["method"] == "resolve_unknown" {
        let facts = policies.verify_execution_resolution_request_facts(body, audience, now_ms)?;
        let encoded: String = connection.query_row(
            "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
            params![
                facts.resource.space.as_str(),
                body["operation_id"]
                    .as_str()
                    .context("resolution operation is missing")?
            ],
            |row| row.get(0),
        )?;
        let actual: vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult =
            serde_json::from_value(body["execution_observation"].clone())?;
        let event: crate::model::Event =
            serde_json::from_str(&actual.handoff.handoff.project_dispatch.canonical_body)?;
        let crate::model::EventCause::Invocation { admission } = event.cause else {
            anyhow::bail!("resolution operation has no original admission");
        };
        ensure!(
            serde_json::from_str::<crate::model::OperationRequest>(&encoded)?
                == serde_json::from_value(admission.source_body["request"].clone())?,
            "resolution observation differs from the durable operation request"
        );
        if facts.context.task.is_some() {
            destination.check_facts(&facts)?;
        }
        return Ok(facts);
    }
    let facts = request_facts_impl(connection, policies, body, audience, Some(destination))?;
    // Non-Task requests bind their complete body after a real Invocation exists.
    // Admission revalidation must use that bound destination before any write.
    if facts.context.task.is_some() {
        destination.check_facts(&facts)?;
    }
    Ok(facts)
}

fn request_facts_impl(
    connection: &Connection,
    policies: &VerifiedPolicySet,
    body: &Value,
    audience: &Audience,
    destination: Option<&vibeface_secure::authorization::prepare::VerifiedExecutionDestination>,
) -> Result<RequestFacts> {
    let text = |field: &str| -> Result<&str> {
        let value = body[field]
            .as_str()
            .with_context(|| format!("{field} is required"))?;
        ensure!(
            !value.is_empty() && value.len() <= 512,
            "invalid request identifier"
        );
        Ok(value)
    };
    let space = SpaceId::parse(text("space_id")?)?;
    let action = crate::model::authorization_action(body)?;
    let method = text("method")?;
    let operation_id = text("authorization_id")?.to_owned();
    let control =
        method == "execution_control" || (method == "task_control" && body["action"] != "status");
    let mut request = if method == "operate" {
        Some(serde_json::from_value::<crate::model::OperationRequest>(
            body["request"].clone(),
        )?)
    } else if control {
        ensure!(
            text("operation_id")? == operation_id,
            "control operation and authorization ID differ"
        );
        let expected = text("expected_operation_id")?;
        let encoded: String = connection.query_row(
            "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
            params![space.as_str(), expected],
            |row| row.get(0),
        )?;
        let original: crate::model::OperationRequest = serde_json::from_str(&encoded)?;
        let admission =
            super::admission::admission_for_operation_from(connection, space.as_str(), expected)?;
        ensure!(
            original.network_id == space.as_str()
                && original.operation_id == expected
                && body["conversation_id"].as_str() == Some(original.conversation_id.as_str())
                && admission.source_body["method"] == "operate"
                && serde_json::from_value::<crate::model::OperationRequest>(
                    admission.source_body["request"].clone()
                )? == original
                && admission.invocation.payload.resource.space == space
                && original.session_id.is_some()
                && matches!(original.action["type"].as_str(), Some("open" | "send")),
            "control changed its real admitted execution operation"
        );
        Some(original)
    } else {
        None
    };
    if let Some(request) = &request {
        ensure!(
            request.network_id == space.as_str()
                && (control || request.operation_id == operation_id),
            "operation and authorization scope differ"
        );
    }
    let mut conversation = request
        .as_ref()
        .map(|request| request.conversation_id.clone())
        .or_else(|| body["conversation_id"].as_str().map(str::to_owned));
    if matches!(method, "operation" | "reconcile" | "resolve_unknown") {
        let encoded: Option<String> = connection
            .query_row(
                "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
                params![space.as_str(), text("operation_id")?],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(encoded) = encoded {
            let operation: crate::model::OperationRequest = serde_json::from_str(&encoded)?;
            if let Some(scope) = &conversation {
                ensure!(
                    scope == &operation.conversation_id,
                    "operation conversation mismatch"
                );
            }
            ensure!(
                operation.network_id == space.as_str()
                    && operation.operation_id == text("operation_id")?,
                "stored operation scope differs"
            );
            conversation = Some(operation.conversation_id.clone());
            request = Some(operation);
        } else {
            ensure!(
                matches!(method, "operation" | "reconcile"),
                "operation is unavailable"
            );
        }
    }
    if method == "ack" {
        let encoded: String = connection.query_row(
            "SELECT record FROM notification_outbox WHERE space_id=? AND notification_id=?",
            params![space.as_str(), text("notification_id")?],
            |row| row.get(0),
        )?;
        let notification: crate::model::Notification = serde_json::from_str(&encoded)?;
        if let Some(scope) = &conversation {
            ensure!(
                scope == &notification.conversation_id,
                "notification conversation mismatch"
            );
        }
        conversation = Some(notification.conversation_id);
    }
    if method == "session" {
        let mut statement = connection.prepare(
            "SELECT request FROM operations WHERE space_id=? AND target_node=? AND session_id=? ORDER BY operation_id",
        )?;
        let rows = statement.query_map(
            params![space.as_str(), text("target_node")?, text("session_id")?],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            let operation: crate::model::OperationRequest = serde_json::from_str(&row?)?;
            if let Some(scope) = &conversation {
                ensure!(
                    scope == &operation.conversation_id,
                    "ambiguous session conversation"
                );
            }
            if let Some(previous) = &request {
                ensure!(
                    previous.task_id == operation.task_id && previous.binding == operation.binding,
                    "ambiguous session task binding"
                );
            }
            conversation = Some(operation.conversation_id.clone());
            request = Some(operation);
        }
        ensure!(
            conversation.is_some(),
            "session has no admitted resource context"
        );
    }
    if let Some(request) = &request {
        if let Some(target) = body["target_node"].as_str() {
            ensure!(
                target == request.target_node,
                "request target differs from admitted operation"
            );
        }
        if let Some(session) = body["session_id"].as_str() {
            ensure!(
                request.session_id.as_deref() == Some(session),
                "request session differs from admitted operation"
            );
        }
        if let Some(task) = body["task_id"].as_str() {
            ensure!(
                request.task_id.as_deref() == Some(task),
                "request task differs from admitted operation"
            );
        }
        if !body["binding"].is_null() {
            ensure!(
                serde_json::from_value::<crate::binding::SignedBinding>(body["binding"].clone())?
                    .eq(request
                        .binding
                        .as_ref()
                        .context("operation binding is missing")?),
                "request binding differs from admitted operation"
            );
        }
        if let Some(cwd) = body["cwd"].as_str() {
            ensure!(
                request
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.binding.open_spec["cwd"].as_str())
                    == Some(cwd),
                "request directory differs from admitted binding"
            );
        }
    }
    let stored_resource: Option<String> = if let Some(scope) = &conversation {
        connection
            .query_row(
                "SELECT resource_id FROM conversations WHERE space_id=? AND conversation_id=?",
                params![space.as_str(), scope],
                |row| row.get(0),
            )
            .optional()?
    } else {
        None
    };
    if conversation.is_some() && method != "create_conversation" {
        ensure!(stored_resource.is_some(), "unknown conversation resource");
    }
    let claimed_resource = ResourceId::parse(text("resource_id")?)?;
    let resource = match stored_resource {
        Some(resource) => {
            ensure!(
                claimed_resource.as_str() == resource,
                "request resource mismatch"
            );
            ResourceId::parse(&resource)?
        }
        None => claimed_resource,
    };
    let record = policies
        .resource(&space, &resource)
        .context("resource is not registered by current space policy")?;
    if method == "create_conversation" {
        ensure!(
            record.payload.kind == ResourceKind::Project,
            "conversations require a project resource"
        );
    }
    ensure!(
        record.payload.host == audience.service,
        "request must target the registered resource host"
    );
    let mut context = InvocationContext {
        conversation: conversation.clone(),
        workspace_resource: Some(resource.clone()),
        ..Default::default()
    };
    let task = request
        .as_ref()
        .and_then(|request| request.task_id.as_deref())
        .or_else(|| body["task_id"].as_str());
    if let Some(task) = task {
        let scope = conversation
            .as_deref()
            .context("task requires a conversation")?;
        if method != "create_task" {
            let mut statement = connection.prepare("SELECT envelope FROM events WHERE space_id=? AND scope_id=? AND status='ready' ORDER BY hash")?;
            let rows = statement.query_map(params![space.as_str(), scope], |row| {
                row.get::<_, String>(0)
            })?;
            let mut creation = None;
            for encoded in rows {
                let envelope: crate::model::SignedEvent = serde_json::from_str(&encoded?)?;
                let event: crate::model::Event = serde_json::from_str(&envelope.canonical_body)?;
                if matches!(&event.body, crate::model::EventBody::TaskCreated { task_id, .. } if task_id == task)
                {
                    ensure!(creation.is_none(), "ambiguous task creation");
                    if let Some(expected) = body["task_ref"].as_str() {
                        ensure!(
                            expected == envelope.hash,
                            "task creation reference mismatch"
                        );
                    }
                    creation = Some(vibeface_secure::authorization::EventRef::parse(
                        &envelope.hash,
                    )?);
                }
            }
            context.task = Some(creation.context("task creation is missing")?);
        }
    }
    if let Some(request) = &request {
        context.execution_session = request.session_id.as_ref().map(|session_id| SessionRef {
            target: destination
                .map_or(&audience.device, |value| &value.audience().device)
                .clone(),
            session_id: session_id.clone(),
        });
        context.executor_kind = request.action["executor"].as_str().map(str::to_owned);
        if context.executor_kind.is_none() && destination.is_some() {
            context.executor_kind = request
                .binding
                .as_ref()
                .and_then(|binding| binding.binding.open_spec["executor"].as_str())
                .map(str::to_owned);
        }
        if context.executor_kind.is_none() && request.session_id.is_some() {
            let encoded: Option<String> = connection.query_row(
                "SELECT record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
                params![space.as_str(), request.target_node, request.session_id], |row| row.get(0)).optional()?;
            if let Some(encoded) = encoded {
                let session: crate::model::ExecutionSession = serde_json::from_str(&encoded)?;
                ensure!(
                    session.network_id == space.as_str()
                        && session.target_node == request.target_node
                        && Some(session.session_id.as_str()) == request.session_id.as_deref(),
                    "execution session projection mismatch"
                );
                context.executor_kind = Some(session.executor);
            }
        }
        if let Some(destination) = destination {
            ensure!(
                crate::crypto::fingerprint(destination.public_key().as_str())?
                    == request.target_node,
                "operation targets another execution destination"
            );
        } else {
            let key: String = connection.query_row(
                "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
                params![space.as_str(), request.target_node],
                |row| row.get(0),
            )?;
            ensure!(
                DeviceId::from_key(&PublicKey::parse(&key)?) == audience.device,
                "operation targets another device"
            );
        }
    } else if matches!(method, "bind_execution" | "rebind_execution") {
        let target_key: String = if let Some(destination) = destination {
            ensure!(
                crate::crypto::fingerprint(destination.public_key().as_str())?
                    == text("target_node")?,
                "binding targets another execution destination"
            );
            destination.public_key().to_string()
        } else {
            connection.query_row(
                "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
                params![space.as_str(), text("target_node")?],
                |row| row.get(0),
            )?
        };
        context.execution_session = Some(SessionRef {
            target: DeviceId::from_key(&PublicKey::parse(&target_key)?),
            session_id: text("session_id")?.to_owned(),
        });
        context.executor_kind = Some(text("executor")?.to_owned());
    } else if method == "seal_execution" {
        let binding: crate::binding::SignedBinding =
            serde_json::from_value(body["binding"].clone())?;
        let target_key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
            params![space.as_str(), binding.binding.target_node],
            |row| row.get(0),
        )?;
        context.execution_session = Some(SessionRef {
            target: DeviceId::from_key(&PublicKey::parse(&target_key)?),
            session_id: binding.binding.session_id,
        });
        context.executor_kind = binding.binding.open_spec["executor"]
            .as_str()
            .map(str::to_owned);
    }
    if !body["sdk_session"].is_null() {
        let session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
        let encoded: String = connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                space.as_str(),
                format!("agent-session:{}", session.session_id)
            ],
            |row| row.get(0),
        )?;
        let saved: Value = serde_json::from_str(&encoded)?;
        ensure!(
            saved["sdk_session"] == body["sdk_session"]
                && saved["parent_invocation"] == body["parent_invocation"]
                && saved["conversation_id"].as_str() == conversation.as_deref(),
            "agent session context mismatch"
        );
        let trigger: String = connection.query_row(
            "SELECT envelope FROM events WHERE space_id=? AND hash=? AND status='ready'",
            params![space.as_str(), session.session_id],
            |row| row.get(0),
        )?;
        let trigger: crate::model::SignedEvent = serde_json::from_str(&trigger)?;
        let trigger: crate::model::Event = serde_json::from_str(&trigger.canonical_body)?;
        if matches!(
            &trigger.body,
            crate::model::EventBody::ExecutionObserved { .. }
        ) {
            let source = match &trigger.cause {
                crate::model::EventCause::Derived { admission, .. } => admission,
                _ => anyhow::bail!("continuation has no original execution admission"),
            };
            ensure!(
                context.task == source.invocation.payload.context.task
                    && context.conversation == source.invocation.payload.context.conversation
                    && method != "create_task",
                "continuation request escaped its original task"
            );
            if method == "session" {
                let original: crate::model::OperationRequest =
                    serde_json::from_value(source.source_body["request"].clone())?;
                ensure!(
                    body["target_node"].as_str() == Some(original.target_node.as_str())
                        && body["session_id"].as_str() == original.session_id.as_deref(),
                    "continuation cannot read another execution session"
                );
            }
        }
        context.sdk_session = Some(session);
    }
    let workspace = request
        .as_ref()
        .and_then(|request| request.action["cwd"].as_str())
        .or_else(|| {
            destination
                .and(request.as_ref())
                .and_then(|request| request.binding.as_ref())
                .and_then(|binding| binding.binding.open_spec["cwd"].as_str())
        })
        .or_else(|| {
            matches!(method, "bind_execution" | "rebind_execution")
                .then(|| body["cwd"].as_str())
                .flatten()
        });
    if let Some(workspace) = workspace {
        if destination.is_some() {
            super::execution_dispatch::validate_remote_resource_path(workspace)?;
        } else {
            let root = resource_root_from(connection, space.as_str(), &resource)?;
            let canonical = std::fs::canonicalize(workspace)?;
            ensure!(
                canonical.to_str() == Some(workspace) && canonical.starts_with(&root),
                "workspace is outside the registered project root"
            );
        }
    }
    Ok(RequestFacts {
        resource: ResourceRef { space, resource },
        action,
        audience: audience.clone(),
        operation_id,
        body_hash: ObjectHash::body(body)?,
        context,
    })
}

fn resource_root_from(
    connection: &Connection,
    space: &str,
    resource: &ResourceId,
) -> Result<std::path::PathBuf> {
    let encoded: String = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, format!("resource-root:{resource}")],
            |row| row.get(0),
        )
        .context("project has no local execution root")?;
    let root: std::path::PathBuf = serde_json::from_str(&encoded)?;
    ensure!(
        root.is_absolute() && root.is_dir() && std::fs::canonicalize(&root)? == root,
        "registered project root changed"
    );
    Ok(root)
}

#[cfg(test)]
pub(crate) fn authenticate_account_from(
    connection: &Connection,
    policy: &super::journal::PolicyState,
    principal: &str,
    params: &Value,
    now_ms: u64,
) -> Result<Value> {
    ensure!(
        params["device_public_key"].as_str() == Some(principal),
        "account device differs from Noise identity"
    );
    let account = AccountId::parse(
        params["account_id"]
            .as_str()
            .context("account_id is required")?,
    )?;
    let space = SpaceId::parse(
        params["space_id"]
            .as_str()
            .context("space_id is required")?,
    )?;
    let key = PublicKey::parse(principal)?;
    let requested_resource: Option<ResourceId> = params
        .get("resource_id")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    let requested_audience: Option<Audience> = params
        .get("audience")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    let requested_grant: Option<ObjectHash> = params
        .get("grant_leaf")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    let mut statement = connection.prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let journal: Vec<JournalEntry> = rows
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    let current = policy.verified().space(&space).context("unknown space")?;
    for leaf in &current.grants {
        if requested_grant
            .as_ref()
            .is_some_and(|expected| expected != leaf)
        {
            continue;
        }
        let grant = policy
            .verified()
            .grant(leaf)
            .context("registered grant proof is missing")?;
        if requested_audience
            .as_ref()
            .is_some_and(|audience| audience != &grant.payload.audience)
        {
            continue;
        }
        let Ok(bundle) = policy.verified().client_authorization_projection(
            policy.pins(),
            &journal,
            &account,
            &space,
            &key,
            leaf,
            now_ms,
        ) else {
            continue;
        };
        let pins = bundle_pins(policy.pins(), &bundle);
        let verified = vibeface_secure::authorization::verify_client_authorization(
            &serde_json::to_string(&bundle)?,
            &serde_json::to_string(&pins)?,
            account.as_str(),
            space.as_str(),
            principal,
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        if requested_resource.as_ref().is_some_and(|resource| {
            !verified
                .resources()
                .iter()
                .any(|record| &record.payload.resource == resource)
        }) {
            continue;
        }
        return Ok(
            json!({"scope":verified.scope(),"authorization":bundle,"pins":pins,"resources":verified.resources()}),
        );
    }
    anyhow::bail!("device has no current account authorization for this space")
}

impl Store {
    #[cfg(test)]
    pub(crate) fn authenticate_account(
        &self,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        authenticate_account_from(&self.connection, &self.policy, principal, params, now_ms)
    }
    pub fn install_policy_journal(&mut self, journal: &[JournalEntry], now_ms: u64) -> Result<()> {
        let update = self.policy.prepare_network_update(journal, now_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = super::journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        tx.commit()?;
        self.policy = state;
        Ok(())
    }

    fn node_claim(
        &mut self,
        identity: &LocalIdentity,
        capability: &str,
        now_ms: u64,
    ) -> Result<Value> {
        self.verify_bootstrap_capability(capability)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_capability: String = tx.query_row(
            "SELECT value FROM identity_meta WHERE key='bootstrap_capability_hash'",
            [],
            |row| row.get(0),
        )?;
        let has_policy: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM policy_pins)", [], |row| {
                row.get(0)
            })?;
        ensure!(
            current_capability == crate::crypto::digest(capability.as_bytes()) && !has_policy,
            "Node claim initialization changed before its transaction"
        );
        let previous: Option<String> = tx
            .query_row(
                "SELECT value FROM identity_meta WHERE key='node_claim'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let pending_possession: Option<String> = tx
            .query_row(
                "SELECT value FROM identity_meta WHERE key='node_possession'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            previous.is_some() || pending_possession.is_none(),
            "pending Node possession has lost its original claim"
        );
        if let Some(previous) = previous {
            let claim: Signed<NodeClaim> = serde_json::from_str(&previous)?;
            ensure!(
                claim.payload.node_public_key.as_str() == identity.public_key_hex()?
                    && claim.payload.claim_nonce.as_str() == capability,
                "stored Node claim differs from this installation"
            );
            if pending_possession.is_some()
                || (claim.payload.issued_at_ms <= now_ms && now_ms < claim.payload.expires_at_ms)
            {
                return Ok(json!({"claim":claim}));
            }
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let encoded = bootstrap::create_node_claim(
            &signer,
            capability,
            now_ms as f64,
            now_ms
                .checked_add(bootstrap::NODE_CLAIM_MAX_AGE_MS)
                .context("claim expiry overflow")? as f64,
        )
        .map_err(anyhow::Error::msg)?;

        tx.execute("INSERT INTO identity_meta(key,value) VALUES('node_claim',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&encoded])?;
        tx.commit()?;
        Ok(json!({"claim":serde_json::from_str::<Value>(&encoded)?}))
    }

    fn node_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let claim: Signed<NodeClaim> = serde_json::from_value(request["claim"].clone())?;
        self.verify_bootstrap_capability(claim.payload.claim_nonce.as_str())?;
        let saved: String = self.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='node_claim'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<Signed<NodeClaim>>(&saved)? == claim,
            "claim is not the current local challenge"
        );
        let request_hash = ObjectHash::body(request)?;
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='node_possession'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["request_hash"] == request_hash.as_str()
                    && previous["principal"] == principal,
                "claim was already bound to another account or device"
            );
            return Ok(json!({"proof":previous["proof"]}));
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let proof = bootstrap::sign_node_possession(
            &serde_json::to_string(request)?,
            &signer,
            claim.payload.claim_nonce.as_str(),
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let receipt = json!({"request_hash":request_hash,"principal":principal,"request":request,"proof":proof});
        self.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES('node_possession',?)",
            [serde_json::to_string(&receipt)?],
        )?;
        Ok(json!({"proof":proof}))
    }

    fn bootstrap_account(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        roots: &[std::path::PathBuf],
        now_ms: u64,
    ) -> Result<Value> {
        let request_hash = ObjectHash::body(params)?;
        let old: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='account_bootstrap'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let old: Value = serde_json::from_str(&old)?;
            ensure!(
                old["request_hash"] == request_hash.as_str() && old["principal"] == principal,
                "Node has already been claimed"
            );
            return Ok(old["response"].clone());
        }
        let pins: Vec<PolicyPin> = serde_json::from_value(params["pins"].clone())?;
        let bundle: ClientAuthorizationBundle =
            serde_json::from_value(params["authorization"].clone())?;
        ensure!(
            pins.len() == 2,
            "account creation must pin exactly one account and personal space"
        );
        let account = pins
            .iter()
            .find_map(|pin| match &pin.domain {
                PolicyDomain::Account(id) => Some(id),
                _ => None,
            })
            .context("account pin is missing")?;
        let space = pins
            .iter()
            .find_map(|pin| match &pin.domain {
                PolicyDomain::Space(id) => Some(id),
                _ => None,
            })
            .context("space pin is missing")?;
        let verified = vibeface_secure::authorization::verify_client_authorization(
            &serde_json::to_string(&bundle)?,
            &serde_json::to_string(&pins)?,
            account.as_str(),
            space.as_str(),
            principal,
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            verified.audience().device == DeviceId::from_key(&key)
                && verified.audience().service == ServiceId::from_key(&key),
            "account targets another Node"
        );
        let possession: String = self.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='node_possession'",
            [],
            |row| row.get(0),
        )?;
        let possession: Value = serde_json::from_str(&possession)?;
        ensure!(
            possession["principal"] == principal,
            "account claimant differs from Noise device"
        );
        let request = &possession["request"];
        let claim: Signed<NodeClaim> = serde_json::from_value(request["claim"].clone())?;
        self.verify_bootstrap_capability(claim.payload.claim_nonce.as_str())?;
        ensure!(
            now_ms < claim.payload.expires_at_ms,
            "account creation challenge expired"
        );
        let mut matching_node = false;
        let mut matching_genesis = false;
        for entry in &bundle.journal {
            match entry {
                JournalEntry::Device(certificate) if certificate.payload.public_key == key => {
                    let mut unsigned = certificate.payload.clone();
                    unsigned.possession = None;
                    ensure!(
                        serde_json::to_value(&unsigned)? == request["node_certificate"]
                            && certificate
                                .payload
                                .possession
                                .as_ref()
                                .map(|proof| proof.as_str())
                                == possession["proof"].as_str(),
                        "Node certificate differs from approved possession request"
                    );
                    matching_node = true;
                }
                JournalEntry::AccountGenesis(document) => {
                    ensure!(
                        serde_json::to_value(document)? == request["account_genesis"],
                        "account genesis differs from approved possession request"
                    );
                    matching_genesis = true;
                }
                _ => {}
            }
        }
        ensure!(
            matching_node && matching_genesis,
            "account bootstrap lacks approved Node enrollment"
        );
        let update = self
            .policy
            .prepare_bootstrap(&pins, &bundle.journal, now_ms)?;
        let response = json!({"scope":verified.scope(),"authorization":bundle,"pins":pins,"resources":verified.resources()});
        let receipt =
            json!({"request_hash":request_hash,"principal":principal,"response":response});
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = super::journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        let selected_root = if let Some(selected) = params["project_root"].as_str() {
            let selected = std::fs::canonicalize(selected)?;
            ensure!(
                roots
                    .iter()
                    .any(|root| std::fs::canonicalize(root).is_ok_and(|root| root == selected)),
                "project root is not configured on this Node"
            );
            Some(selected)
        } else if roots.len() == 1 {
            Some(std::fs::canonicalize(&roots[0])?)
        } else {
            None
        };
        if let Some(root) = selected_root {
            for resource in verified.resources() {
                tx.execute(
                    "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                    params![
                        space.as_str(),
                        format!("resource-root:{}", resource.payload.resource),
                        serde_json::to_string(&root)?
                    ],
                )?;
            }
        }
        let changed = tx.execute("UPDATE identity_meta SET value='consumed:'||value WHERE key='bootstrap_capability_hash' AND value=?", [crate::crypto::digest(claim.payload.claim_nonce.as_str().as_bytes())])?;
        ensure!(changed == 1, "bootstrap capability was already consumed");
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('account_bootstrap',?)",
            [serde_json::to_string(&receipt)?],
        )?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
    pub fn authorization_state(&self) -> &VerifiedPolicySet {
        self.policy.verified()
    }

    pub fn authorize_transport_node(
        &self,
        node_id: &str,
        now_ms: u64,
    ) -> Result<crate::identity::NodeCertificate> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT public_key FROM node_certificates WHERE node_id=?")?;
        let keys = statement
            .query_map([node_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            keys.len() == 1,
            "transport service identity is missing or ambiguous"
        );
        let public_key = PublicKey::parse(&keys[0])?;
        ensure!(
            crate::crypto::fingerprint(public_key.as_str())? == node_id,
            "transport fingerprint differs from its certificate key"
        );
        let certificate = self
            .policy
            .verified()
            .verify_transport_device(&DeviceId::from_key(&public_key), now_ms)?;
        ensure!(
            certificate.payload.public_key == public_key,
            "transport certificate differs from its pinned key"
        );
        Ok(crate::identity::NodeCertificate {
            node_id: node_id.to_owned(),
            public_key: public_key.as_str().to_owned(),
            roles: vec![],
            revoked: false,
        })
    }

    pub fn trusted_policy_pins(&self) -> &[PolicyPin] {
        self.policy.pins()
    }

    pub fn request_facts(&self, body: &Value, audience: &Audience) -> Result<RequestFacts> {
        request_facts_from(&self.connection, self.policy.verified(), body, audience)
    }

    pub fn resource_root(&self, space: &str, resource: &ResourceId) -> Result<std::path::PathBuf> {
        resource_root_from(&self.connection, space, resource)
    }

    pub fn authorized_conversation_ids(&self, auth: &VerifiedAuthorization) -> Result<Vec<String>> {
        self.resource_conversation_ids(auth.resource().space.as_str(), &auth.resource().resource)
    }

    pub fn resource_conversation_ids(
        &self,
        space: &str,
        resource: &ResourceId,
    ) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT conversation_id FROM conversations WHERE space_id=? AND resource_id=? ORDER BY conversation_id",
        )?;
        Ok(statement
            .query_map(params![space, resource.as_str()], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn resource_event_hashes(
        &self,
        space: &str,
        resource: &ResourceId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        ensure!(
            (1..=257).contains(&limit),
            "invalid resource inventory page size"
        );
        let mut statement = self.connection.prepare(
            "SELECT hash FROM events WHERE space_id=? AND resource_id=? AND hash>? ORDER BY hash LIMIT ?",
        )?;
        Ok(statement
            .query_map(
                params![space, resource.as_str(), after.unwrap_or(""), limit],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn resource_blob_allowed(
        &self,
        space: &str,
        resource: &ResourceId,
        hash: &str,
    ) -> Result<bool> {
        let mut statement = self.connection.prepare("SELECT e.envelope FROM events e JOIN json_each(json_extract(e.envelope,'$.canonical_body'),'$.blobs') b WHERE e.space_id=? AND e.resource_id=? AND e.status='ready' AND json_extract(b.value,'$.hash')=? LIMIT 1")?;
        let encoded: Option<String> = statement
            .query_row(params![space, resource.as_str(), hash], |row| row.get(0))
            .optional()?;
        if let Some(encoded) = encoded {
            let envelope = serde_json::from_str(&encoded)?;
            let event = self.verify_event(space, &envelope)?;
            ensure!(
                event.resource_id == *resource,
                "blob resource projection mismatch"
            );
            return Ok(event.blobs.iter().any(|blob| blob.hash == hash));
        }
        Ok(false)
    }

    pub fn authorize_request(
        &self,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        Ok(self
            .policy
            .verified()
            .verify_invocation(invocation, facts, transport, now_ms)?)
    }

    pub fn authorize_request_for_body(
        &self,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        authorize_invocation_for_body(
            self.policy.verified(),
            invocation,
            facts,
            transport,
            body,
            now_ms,
        )
    }

    pub fn issue_freshness(
        &mut self,
        auth: &VerifiedAuthorization,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        self.policy.verified().check_current(auth, now_ms)?;
        let lifetime = if stage == CheckStage::RemoteRead {
            vibeface_secure::authorization::REMOTE_READ_MAX_AGE_MS
        } else {
            vibeface_secure::authorization::EXECUTION_LEASE_MS
        };
        let expires_at_ms = now_ms
            .checked_add(lifetime)
            .context("challenge expiry overflow")?
            .min(auth.expires_at_ms());
        let challenge = FreshnessChallenge::new(
            auth,
            stage,
            vibeface_secure::authorization::Nonce::random()?,
            now_ms,
            expires_at_ms,
        )?;
        let encoded = serde_json::to_string(&challenge)?;
        let stage = serde_json::to_string(&stage)?;
        self.connection.execute(
            "INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,?,?,?)",
            params![auth.resource().space.as_str(), challenge.nonce.as_str(), auth.invocation_hash().as_str(), stage.trim_matches('"'), encoded, expires_at_ms],
        )?;
        Ok(challenge)
    }

    pub fn sign_current_receipts(
        &self,
        challenge: &FreshnessChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        use vibeface_secure::authorization::{DeviceId, PublicKey, ServiceId};
        ensure!(
            challenge.issued_at_ms <= now_ms && now_ms < challenge.expires_at_ms,
            "freshness challenge expired"
        );
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let mut currents = Vec::new();
        for expected in &challenge.policies {
            let state = self.policy.verified();
            let authority = match &expected.domain {
                PolicyDomain::Account(id) => state.account(id).map(|account| &account.authority),
                PolicyDomain::Space(id) => state.space(id).map(|space| &space.authority),
            };
            let Some(authority) = authority.filter(|authority| authority.public_key == key) else {
                continue;
            };
            let current = state
                .head(&expected.domain)
                .context("local authority policy is not installed")?;
            ensure!(
                current == expected,
                "freshness challenge references an outdated local authority policy"
            );
            let (kind, id) = match &expected.domain {
                PolicyDomain::Account(id) => ("account", id.as_str()),
                PolicyDomain::Space(id) => ("space", id.as_str()),
            };
            let persisted: Option<(u64, u64, String)> = self.connection.query_row(
                "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
                params![kind,id],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            ensure!(
                persisted
                    == Some((
                        current.authority_epoch,
                        current.sequence,
                        current.head.to_string()
                    )),
                "persisted authority policy changed before freshness signing"
            );
            ensure!(
                authority.device == DeviceId::from_key(&key)
                    && authority.service == ServiceId::from_key(&key),
                "authority identity mismatch"
            );
            currents.push(current.clone());
        }
        if currents.is_empty() {
            return Ok(Vec::new());
        }
        currents.sort_by(|left, right| left.domain.cmp(&right.domain));
        let receipt = FreshnessReceipt {
            version: vibeface_secure::authorization::AUTH_VERSION,
            currents,
            challenge_hash: challenge.hash()?,
            issued_at_ms: now_ms,
            expires_at_ms: challenge.expires_at_ms,
        };
        receipt.validate_currents()?;
        Ok(vec![Signed::sign(receipt, &signer)?])
    }
}

#[cfg(test)]
fn bundle_pins(pins: &[PolicyPin], bundle: &ClientAuthorizationBundle) -> Vec<PolicyPin> {
    let domains: std::collections::BTreeSet<_> = bundle
        .journal
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::AccountGenesis(document) | JournalEntry::Account(document) => {
                Some(PolicyDomain::Account(document.payload.account.clone()))
            }
            JournalEntry::SpaceGenesis(document) | JournalEntry::Space(document) => {
                Some(PolicyDomain::Space(document.payload.space.clone()))
            }
            JournalEntry::AccountCommit(commit) => Some(PolicyDomain::Account(
                commit.proposal.payload.account.clone(),
            )),
            JournalEntry::SpaceCommit(commit) => {
                Some(PolicyDomain::Space(commit.proposal.payload.space.clone()))
            }
            _ => None,
        })
        .collect();
    pins.iter()
        .filter(|pin| domains.contains(&pin.domain))
        .cloned()
        .collect()
}

pub(crate) fn project_policy_in(
    connection: &Connection,
    policy: &super::journal::PolicyState,
) -> Result<()> {
    let mut statement = connection.prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
    let entries = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str::<JournalEntry>(&row?)?))
        .collect::<Result<Vec<_>>>()?;
    drop(statement);
    for pin in policy.pins() {
        let PolicyDomain::Space(space) = &pin.domain else {
            continue;
        };
        let history = policy
            .verified()
            .space_history(space)
            .context("space projection has no verified history")?;
        let current = history.last().context("space projection is empty")?;
        connection.execute("INSERT INTO spaces(space_id,revision,bundle) VALUES(?,?,?) ON CONFLICT(space_id) DO UPDATE SET revision=excluded.revision,bundle=excluded.bundle",
            params![space.as_str(),current.payload.sequence,serde_json::to_string(current)?])?;
        for entry in &entries {
            match entry {
                JournalEntry::Resource(record) if record.payload.space == *space => {
                    let kind = match record.payload.kind {
                        ResourceKind::Project => "project",
                        ResourceKind::SpaceControl => "space_control",
                        ResourceKind::ExecutionService => "execution_service",
                    };
                    let hash = record.hash()?;
                    connection.execute("INSERT INTO resource_records(space_id,resource_id,kind,host_service_id,record_hash,signed_record) VALUES(?,?,?,?,?,?) ON CONFLICT(space_id,resource_id) DO UPDATE SET kind=excluded.kind,host_service_id=excluded.host_service_id,record_hash=excluded.record_hash,signed_record=excluded.signed_record",
                        params![space.as_str(),record.payload.resource.as_str(),kind,record.payload.host.as_str(),hash.as_str(),serde_json::to_string(record)?])?;
                }
                JournalEntry::Device(certificate)
                    if matches!(certificate.payload.subject, Actor::Service(_)) =>
                {
                    let certificate_hash = certificate.hash()?;
                    let associated = match &certificate.payload.owner {
                        PolicyDomain::Space(owner) => owner == space,
                        PolicyDomain::Account(owner) => matches!(&current.payload.root,
                            vibeface_secure::authorization::SpaceRoot::Personal { owner: account, .. } if account == owner),
                    } || history
                        .iter()
                        .any(|document| document.payload.devices.contains(&certificate_hash));
                    if associated {
                        let node =
                            crate::crypto::fingerprint(certificate.payload.public_key.as_str())?;
                        let old: Option<String> = connection.query_row("SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
                            params![space.as_str(),node], |row| row.get(0)).optional()?;
                        ensure!(
                            old.as_deref()
                                .is_none_or(|old| old == certificate.payload.public_key.as_str()),
                            "historical Node public key changed"
                        );
                        connection.execute("INSERT OR IGNORE INTO node_certificates(space_id,node_id,public_key) VALUES(?,?,?)",
                            params![space.as_str(),node,certificate.payload.public_key.as_str()])?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_resource_projections(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("SELECT space_id,resource_id,kind,host_service_id,record_hash,signed_record FROM resource_records")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (space, resource, kind, host, hash, encoded) = row?;
        let record: Signed<vibeface_secure::authorization::ResourceRecord> =
            serde_json::from_str(&encoded)?;
        let expected_kind = match record.payload.kind {
            ResourceKind::SpaceControl => "space_control",
            ResourceKind::Project => "project",
            ResourceKind::ExecutionService => "execution_service",
        };
        ensure!(
            record.payload.space.as_str() == space
                && record.payload.resource.as_str() == resource
                && record.payload.host.as_str() == host
                && expected_kind == kind
                && record.hash()?.as_str() == hash,
            "stored resource metadata differs from signed record"
        );
        let accepted: String = connection.query_row(
            "SELECT entry FROM policy_journal WHERE entry_hash=?",
            [&hash],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<JournalEntry>(&accepted)? == JournalEntry::Resource(record),
            "resource was not accepted by the policy journal"
        );
    }
    Ok(())
}

impl crate::runtime::Runtime {
    pub async fn identity_command(
        &self,
        noise_public_key: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value> {
        PublicKey::parse(noise_public_key)?;
        if method == "execution_service_register" {
            let verification =
                vibeface_secure::signature_verification::SignatureVerification::new();
            let initial = {
                let mut store = self.store.lock().await;
                verification.run(|| {
                    store.governance_command(
                        &self.identity,
                        noise_public_key,
                        method,
                        params,
                        crate::runtime::now_ms() as u64,
                    )
                })
            };
            match initial {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if !matches!(
                        error.downcast_ref::<vibeface_secure::authorization::AuthorizationError>(),
                        Some(vibeface_secure::authorization::AuthorizationError::NeedFreshness(_))
                    ) {
                        return Err(error);
                    }
                }
            }
            let request: vibeface_secure::authorization::execution_service::ExecutionServiceRegistrationRequest =
                serde_json::from_value(params.clone())?;
            let challenge = {
                let mut store = self.store.lock().await;
                verification.run(|| {
                    store.execution_service_registration_challenge(
                        &self.identity,
                        noise_public_key,
                        params,
                        crate::runtime::now_ms() as u64,
                    )
                })?
            };
            let proof = self
                .collect_freshness_challenge_with_verification(
                    request.intent.payload.source_project.space.as_str(),
                    &challenge,
                    &verification,
                )
                .await?;
            let mut store = self.store.lock().await;
            return verification.run(|| {
                store.governance_command_with_registration_freshness(
                    &self.identity,
                    noise_public_key,
                    method,
                    params,
                    crate::runtime::now_ms() as u64,
                    Some(&proof),
                )
            });
        }
        let mut store = self.store.lock().await;
        let _verification =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let now_ms = crate::runtime::now_ms() as u64;
        match method {
            "status" => Ok(
                json!({"initialized":!store.trusted_policy_pins().is_empty(),"node_id":self.identity.node_id()?,
                "public_key":self.identity.public_key_hex()?,"roles":self.config.roles,"version":env!("CARGO_PKG_VERSION")}),
            ),
            "node_claim" => store.node_claim(
                &self.identity,
                params["capability"]
                    .as_str()
                    .context("connection capability is required")?,
                now_ms,
            ),
            "node_possession" => {
                store.node_possession(&self.identity, noise_public_key, params, now_ms)
            }
            "bootstrap" => store.bootstrap_account(
                &self.identity,
                noise_public_key,
                params,
                &self.config.roots,
                now_ms,
            ),
            "begin_client_policy_read" => store.begin_client_policy_read(
                &self.identity,
                noise_public_key,
                serde_json::from_value(params.clone())?,
                now_ms,
            ),
            "client_policy_page" => {
                store.client_policy_page(&self.identity, noise_public_key, params, now_ms)
            }
            "client_policy_witness" => {
                store.client_policy_witness(&self.identity, noise_public_key, params, now_ms)
            }
            "client_policy_selections" => {
                store.client_policy_selections(&self.identity, noise_public_key, params, now_ms)
            }
            "confirm_client_policy_auth" => {
                store.confirm_client_policy_auth(&self.identity, noise_public_key, params, now_ms)
            }
            "recovery_context" => {
                store.recovery_context(&self.identity, noise_public_key, params, now_ms)
            }
            "recovery_context_page" => {
                store.recovery_context_page(&self.identity, noise_public_key, params, now_ms)
            }
            "recovery_context_witness" => {
                store.recovery_context_witness(&self.identity, noise_public_key, params, now_ms)
            }
            "recovery_status" => {
                store.recovery_status(&self.identity, noise_public_key, params, now_ms)
            }
            "recovery_node_possession" => {
                store.recovery_node_possession(&self.identity, noise_public_key, params, now_ms)
            }
            "recover" => store.recover_account(&self.identity, noise_public_key, params, now_ms),
            "cross_node_recovery_context"
            | "cross_node_recovery_context_page"
            | "cross_node_recovery_context_witness"
            | "cross_node_recovery_status"
            | "cross_node_recovery_import_begin"
            | "cross_node_recovery_import_page"
            | "cross_node_recovery_import_cached_page"
            | "cross_node_recovery_import_finish"
            | "cross_node_recovery_import_resume"
            | "cross_node_recovery_source_possession"
            | "cross_node_recovery_possession"
            | "cross_node_recovery_commit"
            | "cross_node_recovery_install"
            | "cross_node_recovery_install_status" => store.cross_node_recovery_command(
                &self.identity,
                noise_public_key,
                method,
                params,
                now_ms,
            ),
            "authenticate" => store
                .authenticate_confirmed_client_policy(
                    &self.identity,
                    noise_public_key,
                    params,
                    now_ms,
                )
                .map(|(_, response)| response),
            "policy_commit" => {
                let entries: Vec<JournalEntry> = serde_json::from_value(params["journal"].clone())?;
                // Public signed updates prove their own exact authority; new roots are never learned here.
                store.reject_passkey_policy_bypass(
                    &entries,
                    &DeviceId::from_key(&PublicKey::parse(&self.identity.public_key_hex()?)?),
                )?;
                store.install_policy_journal(&entries, now_ms)?;
                Ok(json!({"accepted":true}))
            }
            "enrollment_offer_publish"
            | "enrollment_offer_get"
            | "enrollment_request"
            | "enrollment_pending"
            | "enrollment_approve"
            | "enrollment_possession"
            | "enrollment_status"
            | "account_device_commit" => {
                store.enrollment_command(&self.identity, noise_public_key, method, params, now_ms)
            }
            "enrollment_policy_read_begin" => {
                store.begin_enrollment_policy_read(&self.identity, noise_public_key, params, now_ms)
            }
            "enrollment_policy_read_page" => {
                store.enrollment_policy_page(&self.identity, noise_public_key, params, now_ms)
            }
            "enrollment_policy_read_witness" => {
                store.enrollment_policy_witness(&self.identity, noise_public_key, params, now_ms)
            }
            "foreign_execution_policy_selection" => store.foreign_execution_policy_selection(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "foreign_execution_policy_begin" => store.foreign_execution_policy_begin(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "foreign_execution_policy_page" => store.foreign_execution_policy_page(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "foreign_execution_policy_finish" => store.foreign_execution_policy_finish(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "foreign_execution_policy_approve" => store.foreign_execution_policy_approve(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "remote_execution_policy_delegate" => store.remote_execution_policy_delegate(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "remote_execution_policy_prepare" => store.remote_execution_policy_prepare(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "remote_execution_policy_page" => {
                store.remote_execution_policy_page(&self.identity, noise_public_key, params, now_ms)
            }
            "remote_execution_policy_finish" => store.remote_execution_policy_finish(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "execution_share_policy_prepare" => store.execution_share_policy_prepare(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "execution_share_policy_page" => {
                store.execution_share_policy_page(&self.identity, noise_public_key, params, now_ms)
            }
            "execution_share_policy_finish" => store.execution_share_policy_finish(
                &self.identity,
                noise_public_key,
                params,
                now_ms,
            ),
            "account_node_import_begin"
            | "account_node_import_page"
            | "account_node_import_finish"
            | "account_node_import_resume"
            | "account_node_commit"
            | "account_node_install"
            | "account_node_install_status"
            | "account_node_status" => store.node_enrollment_command(
                &self.identity,
                noise_public_key,
                method,
                params,
                now_ms,
            ),
            "team_create" | "space_policy_commit" => {
                store.governance_command(&self.identity, noise_public_key, method, params, now_ms)
            }
            "execution_service_select" => {
                store.execution_service_select(&self.identity, noise_public_key, params, now_ms)
            }
            "execution_service_install_challenge" => store.execution_service_install_challenge(
                &self.identity,
                noise_public_key,
                params,
                &self.config.roots,
                now_ms,
            ),
            "execution_service_install" => store.execution_service_install(
                &self.identity,
                noise_public_key,
                params,
                &self.config.roots,
                now_ms,
            ),
            "passkey_prepare_registration"
            | "passkey_prepare_device"
            | "passkey_prepare_management"
            | "passkey_verify_device"
            | "passkey_verify_management"
            | "passkey_complete_registration"
            | "passkey_complete_device"
            | "passkey_complete_management" => store.passkey_command(
                &self.config,
                &self.identity,
                noise_public_key,
                method,
                params,
                now_ms,
            ),
            _ => anyhow::bail!("unknown identity method"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};

    #[test]
    fn pending_node_possession_keeps_its_original_expired_claim() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = Store::open(":memory:")?;
        let capability = "a".repeat(64);
        store.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES('bootstrap_capability_hash',?)",
            [crate::crypto::digest(capability.as_bytes())],
        )?;
        let first = store.node_claim(&network.node, &capability, now())?;
        let claim: Signed<NodeClaim> = serde_json::from_value(first["claim"].clone())?;
        let marker =
            json!({"purpose":"cross_node_recovery_v2","claim_hash":claim.hash()?}).to_string();
        store.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES('node_possession',?)",
            [&marker],
        )?;
        assert_eq!(
            store.node_claim(&network.node, &capability, claim.payload.expires_at_ms + 1)?,
            first
        );
        let retained: String = store.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='node_possession'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(retained, marker);
        store
            .connection
            .execute("DELETE FROM identity_meta WHERE key='node_claim'", [])?;
        assert!(
            store
                .node_claim(&network.node, &capability, claim.payload.expires_at_ms + 1)
                .is_err()
        );
        let retained: String = store.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='node_possession'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(retained, marker);
        Ok(())
    }

    #[test]
    fn freshness_signing_rejects_authority_revocation_from_another_connection() -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("freshness-authority.sqlite3");
        let mut stale = network.at(&path)?;
        let body = network.body("create_conversation", "freshness-scope");
        let (authorization, _) = network.authorization(&stale, &body, now())?;
        let challenge = stale.issue_freshness(&authorization, CheckStage::Admit, now())?;
        let receipts = stale.sign_current_receipts(&challenge, &network.node, now())?;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].payload.currents, challenge.policies);
        let mut writer = Store::open(&path)?;
        network.revoke_node(&mut writer)?;
        let error = stale
            .sign_current_receipts(&challenge, &network.node, now())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("persisted authority policy changed"),
            "{error}"
        );
        Ok(())
    }
}
